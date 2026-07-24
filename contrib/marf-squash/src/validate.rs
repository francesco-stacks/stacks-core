// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Producer-side validation: compare a squashed output against the source
//! chainstate it was produced from. Used by `squash` (post-squash check) and by
//! the standalone `validate` subcommand. Requires access to the source
//! chainstate; consumer-side verification of a standalone PCS is `verify`.

use std::path::Path;

use stackslib::chainstate::stacks::db::snapshot::{
    IndexSideTableValidation, SortitionSideTableValidation, validate_burnchain_db,
    validate_clarity_side_tables, validate_epoch2_block_files, validate_index_side_tables,
    validate_microblock_streams, validate_nakamoto_staging_blocks,
    validate_sortition_side_tables_with_boundary, validate_spv_headers,
};
use stackslib::chainstate::stacks::index::MarfTrieId;
use stackslib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, SquashValidationStats};

use crate::layout::TargetPaths;
use crate::ops::{BitcoinAuxFiles, SideTableMode};

/// One MARF validation job: the source/squashed paths, the boundary (`tip` +
/// `squash_height`) the squash anchored on, the side tables to check, and the
/// open-opts for the source DB. Mirrors [`crate::ops::SquashJob`].
pub struct ValidateJob<'a, T: MarfTrieId> {
    pub label: &'a str,
    pub source: &'a TargetPaths,
    pub squashed: &'a TargetPaths,
    pub tip: &'a T,
    pub squash_height: u32,
    /// Run the full leaf-by-leaf comparison in addition to the fast
    /// hash-based check (slow, O(leaf_count)).
    pub full: bool,
    pub side_table_mode: SideTableMode,
    pub open_opts: MARFOpenOpts,
}

/// Validate a single MARF target (trie contents + its side tables).
/// Returns `true` if all validations passed.
pub fn validate_one<T: MarfTrieId>(job: ValidateJob<T>) -> bool {
    let ValidateJob {
        label,
        source,
        squashed,
        tip,
        squash_height,
        full,
        side_table_mode,
        open_opts,
    } = job;

    let validation = validate_marf_or_exit(
        source.db.to_str().unwrap(),
        squashed.db.to_str().unwrap(),
        open_opts,
        tip,
        squash_height,
        full,
    );
    println!("Validation results for {label}:");
    print_validation(&validation);

    let marf_valid = validation.is_valid();

    let side_valid = match &side_table_mode {
        SideTableMode::Clarity => validate_clarity_tables(source, squashed),
        SideTableMode::Index {
            first_bitcoin_height,
            reward_cycle_len,
        } => validate_index_tables(source, squashed, *first_bitcoin_height, *reward_cycle_len),
        SideTableMode::Sortition {
            stacks_tip_boundary,
        } => match validate_sortition_side_tables_with_boundary(
            source.db.to_str().unwrap(),
            squashed.db.to_str().unwrap(),
            Some(stacks_tip_boundary),
        ) {
            Ok(v) => {
                print_sortition_side_table_validation(&v);
                v.is_valid()
            }
            Err(e) => {
                eprintln!("Warning: sortition side-table validation failed: {e:?}");
                false
            }
        },
    };

    marf_valid && side_valid
}

/// Validate the squashed MARF trie against the source. Exits if the validation
/// itself cannot run (as opposed to running and finding a mismatch).
fn validate_marf_or_exit<T: MarfTrieId>(
    source_db: &str,
    squashed_db: &str,
    open_opts: MARFOpenOpts,
    tip: &T,
    squash_height: u32,
    full_leaf_scan: bool,
) -> SquashValidationStats {
    match MARF::validate_squashed_at_height(
        source_db,
        squashed_db,
        open_opts,
        tip,
        squash_height,
        full_leaf_scan,
    ) {
        Ok(stats) => stats,
        Err(e) => {
            eprintln!("Failed to validate squashed MARF: {e:?}");
            std::process::exit(1);
        }
    }
}

/// Validate the Clarity side tables; returns `true` if they match the source.
fn validate_clarity_tables(source: &TargetPaths, squashed: &TargetPaths) -> bool {
    match validate_clarity_side_tables(source.db.to_str().unwrap(), squashed.db.to_str().unwrap()) {
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
}

/// Validate the index side tables; returns `true` if they match the source.
fn validate_index_tables(
    source: &TargetPaths,
    squashed: &TargetPaths,
    first_bitcoin_height: u32,
    reward_cycle_len: u32,
) -> bool {
    match validate_index_side_tables(
        source.db.to_str().unwrap(),
        squashed.db.to_str().unwrap(),
        u64::from(first_bitcoin_height),
        u64::from(reward_cycle_len),
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
}

fn print_validation(stats: &SquashValidationStats) {
    println!("Validation:");
    println!("Archival root present: {}", stats.archival_root_present);
    println!("Archival root matches: {}", stats.archival_root_matches);
    println!("Squash height matches: {}", stats.squash_height_matches);
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
    println!(
        "Per-height block hashes missing: {}",
        stats.block_hash_missing
    );
    println!(
        "Per-height block hash mismatches: {}",
        stats.block_hash_mismatches
    );
    println!(
        "Extra squashed block rows: {}",
        stats.extra_squashed_block_rows
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

fn print_index_side_table_validation(v: &IndexSideTableValidation) {
    println!("Index side-table validation:");
    for (name, ok) in v.checks() {
        println!("  {name}: {ok}");
    }
    println!("  Index side-table valid: {}", v.is_valid());
}

fn print_sortition_side_table_validation(v: &SortitionSideTableValidation) {
    println!("Sortition side-table validation:");
    for (name, ok) in v.checks() {
        println!("  {name}: {ok}");
    }
    println!("  Sortition side-table valid: {}", v.is_valid());
}

/// Validate block data (microblocks, nakamoto, epoch 2.x files) against source.
/// Returns `true` if all checks pass.
pub fn validate_block_data(
    src_index: &str,
    dst_index: &str,
    src_blocks_dir: &Path,
    dst_blocks_dir: &Path,
    src_nakamoto: &Path,
    dst_nakamoto: &Path,
) -> bool {
    println!("Validating block data...");
    let mut valid = true;

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
                valid = false;
            }
        }
        Err(e) => {
            eprintln!("  Microblock validation error: {e:?}");
            valid = false;
        }
    }

    // Nakamoto validation.
    if !dst_nakamoto.exists() || !src_nakamoto.exists() {
        eprintln!(
            "  nakamoto.sqlite missing (src={}, dst={})",
            src_nakamoto.exists(),
            dst_nakamoto.exists()
        );
        valid = false;
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
                    valid = false;
                }
            }
            Err(e) => {
                eprintln!("  Nakamoto validation error: {e:?}");
                valid = false;
            }
        }
    }

    // Epoch 2.x file validation.
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
                valid = false;
            }
        }
        Err(e) => {
            eprintln!("  Epoch 2.x file validation error: {e:?}");
            valid = false;
        }
    }

    valid
}

/// Validate Bitcoin auxiliary files (burnchain.sqlite + headers.sqlite).
/// Returns `true` if all checks pass.
pub fn validate_bitcoin_aux_files(files: BitcoinAuxFiles) -> bool {
    let BitcoinAuxFiles {
        src_bc_db,
        dst_bc_db,
        squashed_sort,
        src_hdr,
        dst_hdr,
        bitcoin_height,
    } = files;

    println!("Validating Bitcoin auxiliary files...");
    let mut valid = true;

    match validate_burnchain_db(
        src_bc_db.to_str().unwrap(),
        dst_bc_db.to_str().unwrap(),
        squashed_sort.to_str().unwrap(),
        bitcoin_height,
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
            if !v.is_valid() {
                valid = false;
            }
        }
        Err(e) => {
            eprintln!("  burnchain.sqlite validation error: {e:?}");
            valid = false;
        }
    }

    match validate_spv_headers(
        src_hdr.to_str().unwrap(),
        dst_hdr.to_str().unwrap(),
        bitcoin_height,
    ) {
        Ok(v) => {
            println!("  spv_headers_match: {}", v.headers_match);
            println!("  spv_chain_work_match: {}", v.chain_work_match);
            println!("  spv_db_config_match: {}", v.db_config_match);
            println!("  spv_no_extra_headers: {}", v.no_extra_headers);
            if !v.is_valid() {
                valid = false;
            }
        }
        Err(e) => {
            eprintln!("  headers.sqlite validation error: {e:?}");
            valid = false;
        }
    }

    valid
}
