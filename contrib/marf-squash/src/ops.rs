use std::fs;
use std::path::Path;

use stackslib::chainstate::stacks::db::snapshot::{
    IndexSideTableValidation, SortitionSideTableValidation, SortitionTipCopyBoundary,
    copy_burnchain_db, copy_index_side_tables, copy_sortition_side_tables_with_boundary,
    copy_spv_headers, validate_burnchain_db, validate_epoch2_block_files,
    validate_index_side_tables, validate_microblock_streams, validate_nakamoto_staging_blocks,
    validate_sortition_side_tables_with_boundary, validate_spv_headers,
};
use stackslib::chainstate::stacks::index::MarfTrieId;
use stackslib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, SquashValidationStats};
use stackslib::clarity_vm::database::snapshot::{
    copy_clarity_side_tables, validate_clarity_side_tables,
};

use crate::cli::TargetPaths;
use crate::util::{die_with_cleanup, ensure_blobs_match};

#[derive(Clone)]
pub enum SideTableMode {
    Clarity,
    Index {
        first_bitcoin_height: u32,
        reward_cycle_len: u32,
    },
    Sortition {
        stacks_tip_boundary: SortitionTipCopyBoundary,
    },
}

impl SideTableMode {
    pub fn label(&self) -> &'static str {
        match self {
            SideTableMode::Clarity => "clarity",
            SideTableMode::Index { .. } => "index",
            SideTableMode::Sortition { .. } => "sortition",
        }
    }
}

/// Squash a single MARF target and copy its side tables. Exits on error.
pub fn squash_and_copy_one<T: MarfTrieId + Send + Sync>(
    label: &str,
    source: &TargetPaths,
    out: &TargetPaths,
    tip: &T,
    squash_height: u32,
    side_table_mode: SideTableMode,
    open_opts: MARFOpenOpts,
) {
    if let Some(ref blobs) = source.blobs {
        ensure_blobs_match(source.db.to_str().unwrap(), blobs.to_str().unwrap());
    }

    if let Some(parent) = out.db.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        eprintln!(
            "Failed to create output directory '{}': {e}",
            parent.display()
        );
        std::process::exit(1);
    }

    let stats = match MARF::squash_to_path(
        source.db.to_str().unwrap(),
        out.db.to_str().unwrap(),
        open_opts,
        tip,
        squash_height,
        label,
    ) {
        Ok(stats) => stats,
        Err(e) => {
            eprintln!("Failed to squash {label} MARF: {e:?}");
            std::process::exit(1);
        }
    };

    let die = |msg: String| -> ! {
        match out.blobs.as_ref() {
            Some(blobs) => die_with_cleanup(&msg, &[&out.db, blobs]),
            None => die_with_cleanup(&msg, &[&out.db]),
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
                Err(e) => die(format!("Failed to copy Clarity side tables: {e:?}")),
            }
        }
        SideTableMode::Index {
            first_bitcoin_height,
            reward_cycle_len,
        } => {
            println!("Copying index side tables...");
            match copy_index_side_tables(
                source.db.to_str().unwrap(),
                out.db.to_str().unwrap(),
                u64::from(*first_bitcoin_height),
                u64::from(*reward_cycle_len),
            ) {
                Ok(st) => {
                    println!(
                        "Index side-table copy complete: block_headers={}, nakamoto_headers={}, payments={}, transactions={}, tenure_events={}, reward_sets={}, signer_stats={}, matured_rewards={}, burnchain_txids={}, epoch_transitions={}, staging_blocks={}, fork_storage={}",
                        st.block_headers_rows,
                        st.nakamoto_block_headers_rows,
                        st.payments_rows,
                        st.transactions_rows,
                        st.nakamoto_tenure_events_rows,
                        st.nakamoto_reward_sets_rows,
                        st.signer_stats_rows,
                        st.matured_rewards_rows,
                        st.burnchain_txids_rows,
                        st.epoch_transitions_rows,
                        st.staging_blocks_rows,
                        st.fork_storage_rows
                    );
                }
                Err(e) => die(format!("Failed to copy index side tables: {e:?}")),
            }
        }
        SideTableMode::Sortition {
            stacks_tip_boundary,
        } => {
            println!("Copying sortition side tables...");
            match copy_sortition_side_tables_with_boundary(
                source.db.to_str().unwrap(),
                out.db.to_str().unwrap(),
                Some(stacks_tip_boundary),
            ) {
                Ok(st) => {
                    println!(
                        "Sortition side-table copy complete: snapshots={}, leader_keys={}, block_commits={}, epochs={}, fork_storage={}",
                        st.snapshots_rows,
                        st.leader_keys_rows,
                        st.block_commits_rows,
                        st.epochs_rows,
                        st.fork_storage_rows
                    );
                }
                Err(e) => die(format!("Failed to copy sortition side tables: {e:?}")),
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

    println!("Squash complete ({label}) at MARF height {squash_height}");
    println!("Node count: {}", stats.node_count);
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
pub fn validate_one<T: MarfTrieId>(
    source: &TargetPaths,
    squashed: &TargetPaths,
    tip: &T,
    squash_height: u32,
    full: bool,
    side_table_mode: SideTableMode,
    open_opts: MARFOpenOpts,
) -> bool {
    let label = side_table_mode.label();
    let validation = validate_or_exit(
        source.db.to_str().unwrap(),
        source.blobs.as_deref().map(|p| p.to_str().unwrap()),
        squashed.db.to_str().unwrap(),
        squashed
            .blobs
            .as_deref()
            .expect("squashed output always has external blobs")
            .to_str()
            .unwrap(),
        open_opts,
        tip,
        squash_height,
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

    let index_side_valid = if let SideTableMode::Index {
        first_bitcoin_height,
        reward_cycle_len,
    } = &side_table_mode
    {
        match validate_index_side_tables(
            source.db.to_str().unwrap(),
            squashed.db.to_str().unwrap(),
            u64::from(*first_bitcoin_height),
            u64::from(*reward_cycle_len),
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

    let sortition_side_valid = if let SideTableMode::Sortition {
        stacks_tip_boundary,
    } = &side_table_mode
    {
        match validate_sortition_side_tables_with_boundary(
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
        }
    } else {
        true
    };

    marf_valid && clarity_side_valid && index_side_valid && sortition_side_valid
}

#[allow(clippy::too_many_arguments)]
fn validate_or_exit<T: MarfTrieId>(
    source_db: &str,
    source_blobs: Option<&str>,
    squashed_db: &str,
    squashed_blobs: &str,
    open_opts: MARFOpenOpts,
    tip: &T,
    squash_height: u32,
    full_leaf_scan: bool,
) -> SquashValidationStats {
    if let Some(blobs) = source_blobs {
        ensure_blobs_match(source_db, blobs);
    }
    ensure_blobs_match(squashed_db, squashed_blobs);

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
    println!("  required_tables_present: {}", v.required_tables_present);
    println!("  canonical_set_in_source: {}", v.canonical_set_in_source);
    println!("  fork_storage_match: {}", v.fork_storage_match);
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
        "  stacks_chain_tips_by_burn_view_match: {}",
        v.stacks_chain_tips_by_burn_view_match
    );
    println!(
        "  stacks_chain_tips_within_stacks_boundary: {}",
        v.stacks_chain_tips_within_stacks_boundary
    );
    println!(
        "  stacks_chain_tips_anchor_match: {}",
        v.stacks_chain_tips_anchor_match
    );
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
pub fn validate_bitcoin_aux_files(
    src_bc_db: &Path,
    dst_bc_db: &Path,
    squashed_sort: &Path,
    src_hdr: &Path,
    dst_hdr: &Path,
    bitcoin_height: u32,
) -> bool {
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

/// Copy Bitcoin auxiliary files (burnchain.sqlite + headers.sqlite).
/// Exits on error.
pub fn copy_bitcoin_aux_files(
    src_bc_db: &Path,
    dst_bc_db: &Path,
    squashed_sort: &Path,
    src_hdr: &Path,
    dst_hdr: &Path,
    bitcoin_height: u32,
) {
    println!("Copying burnchain.sqlite (canonical only)...");
    match copy_burnchain_db(
        src_bc_db.to_str().unwrap(),
        dst_bc_db.to_str().unwrap(),
        squashed_sort.to_str().unwrap(),
        bitcoin_height,
    ) {
        Ok(bc_stats) => {
            println!(
                "  block_headers={}, block_ops={}, commit_metadata={}, anchor_blocks={}, overrides={}",
                bc_stats.block_headers_rows,
                bc_stats.block_ops_rows,
                bc_stats.block_commit_metadata_rows,
                bc_stats.anchor_blocks_rows,
                bc_stats.overrides_rows,
            );
        }
        Err(e) => die_with_cleanup(
            &format!("Failed to copy burnchain.sqlite: {e:?}"),
            &[dst_bc_db],
        ),
    }

    println!("Copying headers.sqlite (SPV, up to Bitcoin height {bitcoin_height})...");
    match copy_spv_headers(
        src_hdr.to_str().unwrap(),
        dst_hdr.to_str().unwrap(),
        bitcoin_height,
    ) {
        Ok(spv_stats) => {
            println!(
                "  headers={}, chain_work={}",
                spv_stats.headers_rows, spv_stats.chain_work_rows
            );
        }
        Err(e) => die_with_cleanup(&format!("Failed to copy headers.sqlite: {e:?}"), &[dst_hdr]),
    };
}
