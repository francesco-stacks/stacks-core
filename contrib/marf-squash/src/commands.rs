use blockstack_lib::chainstate::stacks::db::snapshot::{
    copy_confirmed_epoch2_microblocks, copy_epoch2_block_files, copy_nakamoto_staging_blocks,
};
use blockstack_lib::chainstate::stacks::index::marf::{MarfConnection, MARF};
use blockstack_lib::chainstate::stacks::index::storage::TrieFileStorage;
use blockstack_lib::chainstate::stacks::index::trie_sql;
use stacks_common::types::chainstate::{SortitionId, StacksBlockId};

use crate::cli::{BlocksSection, LatestHeightArgs, SquashArgs, ValidateArgs, VerifyArgs};
use crate::manifest::generate_manifest;
use crate::ops::{
    copy_bitcoin_aux_files, squash_and_copy_one, validate_bitcoin_aux_files, validate_block_data,
    validate_one, SideTableMode,
};
use crate::util::{
    bitcoin_height_to_sortition_marf_height, build_pox_constants, chainstate_paths,
    ensure_blobs_match, ensure_flag_requires, ensure_targets_selected,
    find_tenure_end_stacks_height, index_pox_constants, read_db_config, read_first_burn_height,
    selected_targets, sortition_open_opts, squash_marf_open_opts, target_out_paths,
    target_out_paths_sortition, warn_if_in_prepare_phase,
};
use crate::verify::verify_gss;

pub fn run_squash(args: SquashArgs) {
    ensure_targets_selected(
        args.clarity,
        args.index,
        args.sortition,
        args.blocks,
        args.bitcoin,
        args.all,
    );

    let paths = chainstate_paths(&args.chainstate);
    let (do_clarity, do_index, do_sortition) =
        selected_targets(args.clarity, args.index, args.sortition, args.all);

    let bitcoin_height = args.tenure_start_bitcoin_height;

    // Read network config.
    let (mainnet, chain_id) = read_db_config(&paths.index.db);
    let pox = build_pox_constants(mainnet);

    // Derive chainstate root: paths.index.db = ".../chainstate/vm/index.sqlite"
    let chainstate_root = paths
        .index
        .db
        .parent() // .../chainstate/vm
        .and_then(|p| p.parent()) // .../chainstate
        .expect("cannot derive chainstate root from index path");

    // Derive the sortition DB directory path (parent of marf.sqlite).
    let sortition_db_dir = paths
        .sortition
        .db
        .parent()
        .expect("cannot derive sortition dir from sortition db path");

    // Find the Stacks height at the end of this tenure.
    let stacks_height = find_tenure_end_stacks_height(
        chainstate_root.to_str().unwrap(),
        sortition_db_dir.to_str().unwrap(),
        bitcoin_height,
        mainnet,
        chain_id,
        pox.clone(),
    )
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    eprintln!(
        "Squash at tenure start Bitcoin height {bitcoin_height}, \
         Stacks tenure end height {stacks_height}"
    );

    // Prepare-phase warning.
    let first_burn_height = read_first_burn_height(paths.sortition.db.to_str().unwrap());
    warn_if_in_prepare_phase(bitcoin_height, &pox, first_burn_height);

    // Sortition MARF height.
    let sortition_marf_height = bitcoin_height_to_sortition_marf_height(
        paths.sortition.db.to_str().unwrap(),
        bitcoin_height,
    );

    let mut clarity_out = None;
    let mut index_out = None;
    let mut sortition_out = None;

    // Phase 1: Squash & Copy

    if do_clarity {
        let out = target_out_paths(&args.out_dir, &paths.clarity.db);
        squash_and_copy_one(
            "clarity",
            &paths.clarity,
            &out,
            stacks_height,
            SideTableMode::Clarity,
            squash_marf_open_opts(),
        );
        clarity_out = Some(out);
    }

    // Derive PoX constants from the source index DB (needed for index side-table filtering).
    let (pox_first_burn_height, reward_cycle_len) = if do_index {
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
            stacks_height,
            SideTableMode::Index {
                first_burn_height: pox_first_burn_height,
                reward_cycle_len,
            },
            squash_marf_open_opts(),
        );
        index_out = Some(out);
    }

    if do_sortition {
        let out = target_out_paths_sortition(&args.out_dir, &paths.sortition.db);
        squash_and_copy_one(
            "sortition",
            &paths.sortition,
            &out,
            sortition_marf_height,
            SideTableMode::Sortition,
            sortition_open_opts(),
        );
        sortition_out = Some((out, sortition_marf_height));
    }

    // Block preservation: requires --index.
    let do_blocks = args.blocks || args.all;
    ensure_flag_requires("blocks", do_blocks, "index", do_index);

    let mut blocks_stats: Option<BlocksSection> = None;
    let mut copied_block_rel_paths: Vec<String> = Vec::new();

    // These variables are needed by both the copy and validation phases for blocks.
    let src_blocks_dir = args.chainstate.join("chainstate/blocks");
    let dst_blocks_dir = args.out_dir.join("chainstate/blocks");
    let src_nakamoto = args.chainstate.join("chainstate/blocks/nakamoto.sqlite");
    let dst_nakamoto = dst_blocks_dir.join("nakamoto.sqlite");

    if do_blocks {
        // Ensure destination blocks directory exists before any copy step.
        std::fs::create_dir_all(&dst_blocks_dir).unwrap_or_else(|e| {
            eprintln!(
                "Failed to create blocks dir {}: {e}",
                dst_blocks_dir.display()
            );
            std::process::exit(1);
        });

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

    // Bitcoin auxiliary files: burnchain.sqlite + headers.sqlite.
    // Requires --sortition (or --all) for the squashed sortition DB and burn heights.
    let do_bitcoin_aux = args.bitcoin || args.all;
    ensure_flag_requires("bitcoin", do_bitcoin_aux, "sortition", do_sortition);
    // These variables are needed by both the copy and validation phases for bitcoin aux.
    let src_bc_db = args.chainstate.join("burnchain/burnchain.sqlite");
    let dst_bc_db = args.out_dir.join("burnchain/burnchain.sqlite");
    let squashed_sort = args.out_dir.join("burnchain/sortition/marf.sqlite");
    let src_hdr = args.chainstate.join("headers.sqlite");
    let dst_hdr = args.out_dir.join("headers.sqlite");

    if do_bitcoin_aux {
        copy_bitcoin_aux_files(
            &src_bc_db,
            &dst_bc_db,
            &squashed_sort,
            &src_hdr,
            &dst_hdr,
            bitcoin_height as u32,
        );
    }

    // Phase 2: Validation

    let mut all_valid = true;

    if !args.skip_validate {
        println!("--- Validation phase ---");

        if do_clarity
            && !validate_one(
                "clarity",
                &paths.clarity,
                clarity_out.as_ref().unwrap(),
                stacks_height,
                args.full,
                SideTableMode::Clarity,
                squash_marf_open_opts(),
            )
        {
            all_valid = false;
        }

        if do_index
            && !validate_one(
                "index",
                &paths.index,
                index_out.as_ref().unwrap(),
                stacks_height,
                args.full,
                SideTableMode::Index {
                    first_burn_height: pox_first_burn_height,
                    reward_cycle_len,
                },
                squash_marf_open_opts(),
            )
        {
            all_valid = false;
        }

        if do_sortition {
            let (_, marf_height) = sortition_out.as_ref().unwrap();
            if !validate_one(
                "sortition",
                &paths.sortition,
                &sortition_out.as_ref().unwrap().0,
                *marf_height,
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
            if !validate_block_data(
                paths.index.db.to_str().unwrap(),
                i_out.db.to_str().unwrap(),
                &src_blocks_dir,
                &dst_blocks_dir,
                &src_nakamoto,
                &dst_nakamoto,
            ) {
                all_valid = false;
            }
        }

        if do_bitcoin_aux
            && !validate_bitcoin_aux_files(
                &src_bc_db,
                &dst_bc_db,
                &squashed_sort,
                &src_hdr,
                &dst_hdr,
                bitcoin_height as u32,
            )
        {
            all_valid = false;
        }
    }

    if !all_valid {
        eprintln!("Validation failed for one or more targets");
        std::process::exit(1);
    }

    // Generate manifest only for a complete GSS (all MARFs + blocks + bitcoin aux).
    if do_clarity && do_index && do_sortition && do_blocks && do_bitcoin_aux {
        let (sort_paths, sort_height) = sortition_out.unwrap();
        generate_manifest(
            &args.out_dir,
            clarity_out.as_ref().unwrap(),
            index_out.as_ref().unwrap(),
            (&sort_paths, sort_height),
            stacks_height,
            bitcoin_height,
            blocks_stats.unwrap(),
            &copied_block_rel_paths,
        );
    }
}

pub fn run_validate(args: ValidateArgs) {
    ensure_targets_selected(
        args.clarity,
        args.index,
        args.sortition,
        args.blocks,
        args.bitcoin,
        args.all,
    );

    let source_paths = chainstate_paths(&args.source_chainstate);
    let squashed_paths = chainstate_paths(&args.squashed_chainstate);
    let (do_clarity, do_index, do_sortition) =
        selected_targets(args.clarity, args.index, args.sortition, args.all);

    let bitcoin_height = args.tenure_start_bitcoin_height;

    // Resolve the same way as run_squash.
    let (mainnet, chain_id) = read_db_config(&source_paths.index.db);
    let pox = build_pox_constants(mainnet);
    let chainstate_root = source_paths
        .index
        .db
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot derive chainstate root from index path");
    let sortition_db_dir = source_paths
        .sortition
        .db
        .parent()
        .expect("cannot derive sortition dir");

    let stacks_height = find_tenure_end_stacks_height(
        chainstate_root.to_str().unwrap(),
        sortition_db_dir.to_str().unwrap(),
        bitcoin_height,
        mainnet,
        chain_id,
        pox,
    )
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let sortition_marf_height = bitcoin_height_to_sortition_marf_height(
        source_paths.sortition.db.to_str().unwrap(),
        bitcoin_height,
    );

    let mut all_valid = true;

    if do_clarity
        && !validate_one(
            "clarity",
            &source_paths.clarity,
            &squashed_paths.clarity,
            stacks_height,
            args.full,
            SideTableMode::Clarity,
            squash_marf_open_opts(),
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
            stacks_height,
            args.full,
            SideTableMode::Index {
                first_burn_height,
                reward_cycle_len,
            },
            squash_marf_open_opts(),
        ) {
            all_valid = false;
        }
    }

    if do_sortition
        && !validate_one(
            "sortition",
            &source_paths.sortition,
            &squashed_paths.sortition,
            sortition_marf_height,
            args.full,
            SideTableMode::Sortition,
            sortition_open_opts(),
        )
    {
        all_valid = false;
    }

    // Block validation.
    let do_blocks = args.blocks || args.all;
    ensure_flag_requires("blocks", do_blocks, "index", do_index);
    if do_blocks {
        let src_nakamoto = args
            .source_chainstate
            .join("chainstate/blocks/nakamoto.sqlite");
        let dst_nakamoto = args
            .squashed_chainstate
            .join("chainstate/blocks/nakamoto.sqlite");
        let src_blocks_dir = args.source_chainstate.join("chainstate/blocks");
        let dst_blocks_dir = args.squashed_chainstate.join("chainstate/blocks");
        if !validate_block_data(
            source_paths.index.db.to_str().unwrap(),
            squashed_paths.index.db.to_str().unwrap(),
            &src_blocks_dir,
            &dst_blocks_dir,
            &src_nakamoto,
            &dst_nakamoto,
        ) {
            all_valid = false;
        }
    }

    // Bitcoin auxiliary validation.
    let do_bitcoin_aux = args.bitcoin || args.all;
    ensure_flag_requires("bitcoin", do_bitcoin_aux, "sortition", do_sortition);
    if do_bitcoin_aux {
        let btc_height = bitcoin_height as u32;

        let src_bc_db = args.source_chainstate.join("burnchain/burnchain.sqlite");
        let dst_bc_db = args.squashed_chainstate.join("burnchain/burnchain.sqlite");
        let squashed_sort = args
            .squashed_chainstate
            .join("burnchain/sortition/marf.sqlite");
        let src_hdr = args.source_chainstate.join("headers.sqlite");
        let dst_hdr = args.squashed_chainstate.join("headers.sqlite");

        if !validate_bitcoin_aux_files(
            &src_bc_db,
            &dst_bc_db,
            &squashed_sort,
            &src_hdr,
            &dst_hdr,
            btc_height,
        ) {
            all_valid = false;
        }
    }

    if !all_valid {
        eprintln!("Validation failed for one or more targets");
        std::process::exit(1);
    }
}

pub fn run_verify(args: VerifyArgs) {
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

pub fn run_latest_height(args: LatestHeightArgs) {
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

    let open_opts = squash_marf_open_opts();
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
