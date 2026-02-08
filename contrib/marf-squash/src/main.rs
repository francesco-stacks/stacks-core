use std::fs;
use std::path::PathBuf;

use blockstack_lib::chainstate::stacks::index::marf::{
    MARFOpenOpts, MarfConnection, SquashValidationStats, MARF,
};
use blockstack_lib::chainstate::stacks::index::storage::{
    TrieFileStorage, TrieHashCalculationMode,
};
use blockstack_lib::chainstate::stacks::index::trie_sql;
use clap::{Parser, Subcommand};
use stacks_common::types::chainstate::StacksBlockId;

/// Offline squashing CLI for an Index MARF snapshot.
#[derive(Parser, Debug)]
#[command(
    name = "marf-squash",
    about = "Offline squashing tool for the Index MARF"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a squashed MARF and validate against the source.
    Squash(SquashArgs),
    /// Validate a squashed MARF against a source chainstate at a height.
    Validate(ValidateArgs),
    /// Print the latest confirmed block height in the MARF.
    LatestHeight(LatestHeightArgs),
}

/// Arguments for generating a squashed MARF.
#[derive(Parser, Debug)]
struct SquashArgs {
    /// Path to the Index MARF SQLite file (e.g. index.sqlite).
    #[arg(long, value_name = "PATH")]
    db: PathBuf,
    /// Path to the Index MARF blobs file (e.g. index.sqlite.blobs).
    #[arg(long, value_name = "PATH")]
    blobs: PathBuf,
    /// Block height to squash to.
    #[arg(long, value_name = "HEIGHT")]
    height: u32,
    /// Output directory for the squashed MARF files.
    #[arg(long = "out-dir", value_name = "DIR")]
    out_dir: PathBuf,
    /// Skip validation to speed up size measurements.
    #[arg(long = "skip-validate")]
    skip_validate: bool,
    /// Run full leaf-by-leaf comparison (slow, O(leaf_count)).
    /// By default, validation uses the fast hash-based check.
    #[arg(long)]
    full: bool,
}

/// Arguments for validating a squashed MARF against a source.
#[derive(Parser, Debug)]
struct ValidateArgs {
    /// Path to the source Index MARF SQLite file.
    #[arg(long = "source-db", value_name = "PATH")]
    source_db: PathBuf,
    /// Path to the source Index MARF blobs file.
    #[arg(long = "source-blobs", value_name = "PATH")]
    source_blobs: PathBuf,
    /// Path to the squashed Index MARF SQLite file.
    #[arg(long = "squashed-db", value_name = "PATH")]
    squashed_db: PathBuf,
    /// Path to the squashed Index MARF blobs file.
    #[arg(long = "squashed-blobs", value_name = "PATH")]
    squashed_blobs: PathBuf,
    /// Block height to validate at.
    #[arg(long, value_name = "HEIGHT")]
    height: u32,
    /// Run full leaf-by-leaf comparison (slow, O(leaf_count)).
    /// By default, validation uses the fast hash-based check.
    #[arg(long)]
    full: bool,
}

/// Arguments for reporting the latest confirmed height.
#[derive(Parser, Debug)]
struct LatestHeightArgs {
    /// Path to the Index MARF SQLite file.
    #[arg(long, value_name = "PATH")]
    db: PathBuf,
    /// Path to the Index MARF blobs file.
    #[arg(long, value_name = "PATH")]
    blobs: PathBuf,
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
    let db_path = args.db;
    let blobs_path = args.blobs;
    let height = args.height;
    let out_dir = args.out_dir;

    ensure_blobs_match(db_path.to_str().unwrap(), blobs_path.to_str().unwrap());

    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!(
            "Failed to create output directory '{}': {e}",
            out_dir.display()
        );
        std::process::exit(1);
    }

    let db_file_name = match db_path.file_name() {
        Some(name) => name,
        None => {
            eprintln!("Invalid --db path '{}'", db_path.display());
            std::process::exit(1);
        }
    };
    let out_db_path = out_dir.join(db_file_name);

    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = true;

    let stats = match MARF::<StacksBlockId>::squash_to_path(
        db_path.to_str().unwrap(),
        out_db_path.to_str().unwrap(),
        open_opts.clone(),
        height,
    ) {
        Ok(stats) => stats,
        Err(e) => {
            eprintln!("Failed to squash MARF: {e:?}");
            std::process::exit(1);
        }
    };

    let out_blobs_path = PathBuf::from(format!("{}.blobs", out_db_path.display()));

    let validation = if args.skip_validate {
        None
    } else {
        Some(validate_or_exit(
            db_path.to_str().unwrap(),
            blobs_path.to_str().unwrap(),
            out_db_path.to_str().unwrap(),
            out_blobs_path.to_str().unwrap(),
            open_opts,
            height,
            args.full,
        ))
    };

    let original_db_size = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let original_blobs_size = fs::metadata(&blobs_path).map(|m| m.len()).unwrap_or(0);
    let squashed_db_size = fs::metadata(&out_db_path).map(|m| m.len()).unwrap_or(0);
    let squashed_blobs_size = fs::metadata(&out_blobs_path).map(|m| m.len()).unwrap_or(0);

    let original_total = original_db_size + original_blobs_size;
    let squashed_total = squashed_db_size + squashed_blobs_size;
    let savings = original_total.saturating_sub(squashed_total);
    let savings_pct = if original_total == 0 {
        0.0
    } else {
        (savings as f64 / original_total as f64) * 100.0
    };

    println!("Squash complete at height {height}");
    println!("Leaf count: {}", stats.leaf_count);
    println!(
        "Original: db={} bytes, blobs={} bytes, total={} bytes",
        original_db_size, original_blobs_size, original_total
    );
    println!(
        "Squashed: db={} bytes, blobs={} bytes, total={} bytes",
        squashed_db_size, squashed_blobs_size, squashed_total
    );
    println!("Savings: {} bytes ({:.2}%)", savings, savings_pct);
    println!("Output db: {}", out_db_path.display());
    println!("Output blobs: {}", out_blobs_path.display());
    match validation {
        Some(validation) => print_validation(&validation),
        None => println!("Validation skipped"),
    }
}

fn print_validation(stats: &SquashValidationStats) {
    println!("Validation:");
    println!(
        "Root hash at squash height: match={}",
        stats.root_hash_matches
    );
    println!("  source:   {}", stats.source_root_hash);
    println!("  squashed: {}", stats.squashed_root_hash);
    println!("Squash root present: {}", stats.squash_root_present);
    println!("Squash root matches: {}", stats.squash_root_matches);
    println!("Per-height root hashes missing: {}", stats.root_hash_missing);
    println!(
        "Per-height root hash mismatches: {}",
        stats.root_hash_mismatches
    );
    println!(
        "Blob offset mismatches: {}",
        stats.blob_offset_mismatches
    );
    if stats.source_keys_checked > 0 || stats.squashed_keys_checked > 0 {
        println!("Full leaf scan:");
        println!("  Source keys checked: {}", stats.source_keys_checked);
        println!("  Squashed keys checked: {}", stats.squashed_keys_checked);
        println!("  Missing in squashed: {}", stats.missing_in_squashed);
        println!("  Missing in source: {}", stats.missing_in_source);
        println!("  Value mismatches: {}", stats.value_mismatches);
    }
}

fn run_validate(args: ValidateArgs) {
    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = true;

    let validation = validate_or_exit(
        args.source_db.to_str().unwrap(),
        args.source_blobs.to_str().unwrap(),
        args.squashed_db.to_str().unwrap(),
        args.squashed_blobs.to_str().unwrap(),
        open_opts,
        args.height,
        args.full,
    );
    print_validation(&validation);
}

fn run_latest_height(args: LatestHeightArgs) {
    let db_path = args.db;
    let blobs_path = args.blobs;

    ensure_blobs_match(db_path.to_str().unwrap(), blobs_path.to_str().unwrap());

    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = true;

    let src_storage = match TrieFileStorage::open_readonly(db_path.to_str().unwrap(), open_opts) {
        Ok(storage) => storage,
        Err(e) => {
            eprintln!("Failed to open MARF: {e:?}");
            std::process::exit(1);
        }
    };
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

fn ensure_blobs_match(db_path: &str, blobs_path: &str) {
    let expected_blobs = PathBuf::from(format!("{db_path}.blobs"));
    if expected_blobs != PathBuf::from(blobs_path) {
        eprintln!(
            "Expected blobs path '{}' to match '{}'",
            blobs_path,
            expected_blobs.display()
        );
        std::process::exit(1);
    }
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
            "--db",
            "/tmp/index.sqlite",
            "--blobs",
            "/tmp/index.sqlite.blobs",
            "--height",
            "123",
            "--out-dir",
            "/tmp/out",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Squash(args) => {
                assert_eq!(args.db, PathBuf::from("/tmp/index.sqlite"));
                assert_eq!(args.blobs, PathBuf::from("/tmp/index.sqlite.blobs"));
                assert_eq!(args.height, 123);
                assert_eq!(args.out_dir, PathBuf::from("/tmp/out"));
            }
            _ => panic!("expected squash command"),
        }
    }

    #[test]
    fn test_parse_validate_args_ok() {
        let args = vec![
            "marf-squash",
            "validate",
            "--source-db",
            "/tmp/source.sqlite",
            "--source-blobs",
            "/tmp/source.sqlite.blobs",
            "--squashed-db",
            "/tmp/squashed.sqlite",
            "--squashed-blobs",
            "/tmp/squashed.sqlite.blobs",
            "--height",
            "456",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Validate(ValidateArgs {
                source_db,
                source_blobs,
                squashed_db,
                squashed_blobs,
                height,
                ..
            }) => {
                assert_eq!(source_db, PathBuf::from("/tmp/source.sqlite"));
                assert_eq!(source_blobs, PathBuf::from("/tmp/source.sqlite.blobs"));
                assert_eq!(squashed_db, PathBuf::from("/tmp/squashed.sqlite"));
                assert_eq!(squashed_blobs, PathBuf::from("/tmp/squashed.sqlite.blobs"));
                assert_eq!(height, 456);
            }
            _ => panic!("expected validate command"),
        }
    }

    #[test]
    fn test_parse_latest_height_args_ok() {
        let args = vec![
            "marf-squash",
            "latest-height",
            "--db",
            "/tmp/index.sqlite",
            "--blobs",
            "/tmp/index.sqlite.blobs",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::LatestHeight(LatestHeightArgs { db, blobs }) => {
                assert_eq!(db, PathBuf::from("/tmp/index.sqlite"));
                assert_eq!(blobs, PathBuf::from("/tmp/index.sqlite.blobs"));
            }
            _ => panic!("expected latest-height command"),
        }
    }

    #[test]
    fn test_parse_args_from_missing() {
        let args = vec!["marf-squash", "squash", "--db", "/tmp/index.sqlite"]
            .into_iter()
            .map(String::from);
        assert!(Cli::try_parse_from(args).is_err());
    }
}
