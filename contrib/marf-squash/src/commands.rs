use stackslib::chainstate::stacks::db::snapshot::{
    SortitionTipCopyBoundary, copy_confirmed_epoch2_microblocks, copy_epoch2_block_files,
    copy_nakamoto_staging_blocks,
};
use stackslib::chainstate::stacks::index::storage::SquashBoundary;

use crate::cli::{BlocksSection, SquashArgs, ValidateArgs, VerifyArgs};
use crate::manifest::generate_manifest;
use crate::ops::{
    SideTableMode, copy_bitcoin_aux_files, squash_and_copy_one, validate_bitcoin_aux_files,
    validate_block_data, validate_one,
};
use crate::util::{
    build_pox_constants, chainstate_paths, die_with_cleanup, enforce_minimum_tenure_height,
    ensure_flag_requires, ensure_targets_selected, read_db_config,
    resolve_canonical_squash_targets, selected_targets, sortition_open_opts_for_path,
    squash_marf_open_opts, target_out_paths, target_out_paths_sortition, warn_if_in_prepare_phase,
};
use crate::verify::verify_gss;

fn sortition_tip_copy_boundary(
    targets: &crate::util::CanonicalSquashTargets,
) -> SortitionTipCopyBoundary {
    SortitionTipCopyBoundary {
        max_stacks_height: u64::from(targets.stacks_height),
        anchor_consensus_hash: targets.stacks_tip_consensus_hash.clone(),
        anchor_burn_view_consensus_hash: targets.stacks_tip_burn_view_consensus_hash.clone(),
        anchor_block_hash: targets.stacks_tip_block_hash.clone(),
        anchor_block_height: u64::from(targets.stacks_height),
    }
}

/// Network subdirectories marf-squash can target. A squash requires epoch 3.4
/// (Nakamoto), so only the persistent Nakamoto networks qualify: mainnet and
/// the krypton testnet.
const KNOWN_NETWORK_SUBDIRS: &[&str] = &["mainnet", "krypton"];

fn is_known_network(subdir: &str) -> bool {
    KNOWN_NETWORK_SUBDIRS.contains(&subdir)
}

/// Resolve the `working_dir/<network>/` subdirectory for the squash output.
///
/// Mainnet is unambiguous. For any other network the mode (`krypton`, `xenon`,
/// ...) is not recoverable from the chainstate alone (testnet/regtest share a
/// chain id), so mirror the source chainstate's own subdirectory name.
fn network_subdir(mainnet: bool, chainstate: &std::path::Path) -> Result<String, String> {
    if mainnet {
        return Ok("mainnet".to_string());
    }
    let subdir = chainstate
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "cannot derive the network subdirectory from --chainstate '{}'; \
                 pass a path ending in the network name, e.g. /stacks/krypton",
                chainstate.display()
            )
        })?;
    if subdir == "mainnet" {
        return Err(format!(
            "--chainstate '{}' ends in 'mainnet' but the chainstate is not mainnet; \
             refusing to write a 'mainnet' subdirectory for a testnet/regtest chainstate",
            chainstate.display()
        ));
    }
    if !is_known_network(&subdir) {
        return Err(format!(
            "'{subdir}' is not a recognized stacks-node network ({}); stacks-node would reject \
             this mode. Point --chainstate at a path ending in the network name.",
            KNOWN_NETWORK_SUBDIRS.join(", ")
        ));
    }
    Ok(subdir)
}

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

    let tenure_start_bitcoin_height = args.tenure_start_bitcoin_height;

    let (mainnet, chain_id) = read_db_config(&paths.index.db);

    let subdir = network_subdir(mainnet, &args.chainstate).unwrap_or_else(|e| {
        eprintln!("Error: {e}");
        std::process::exit(1);
    });
    let out_root = args.out_dir.join(&subdir);

    // Require the destination network dir to be absent or empty. Re-running into
    // an existing tree can leave partial or duplicate data (e.g. nakamoto.sqlite
    // rows inserted twice) that is difficult to diagnose.
    if out_root.exists() {
        let is_empty = std::fs::read_dir(&out_root)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !is_empty {
            eprintln!(
                "Error: output '{}' already exists and is not empty.\n\
                 Remove it or choose a different --out-dir to avoid partial/duplicate output.",
                out_root.display()
            );
            std::process::exit(1);
        }
    }
    std::fs::create_dir_all(&out_root).unwrap_or_else(|e| {
        eprintln!("Failed to create output dir {}: {e}", out_root.display());
        std::process::exit(1);
    });

    let pox = build_pox_constants(mainnet, args.config.as_deref());

    // A squashed snapshot is only usable from epoch 3.4 onwards.
    enforce_minimum_tenure_height(tenure_start_bitcoin_height, mainnet, args.config.as_deref());

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

    let targets = resolve_canonical_squash_targets(
        chainstate_root.to_str().unwrap(),
        sortition_db_dir.to_str().unwrap(),
        tenure_start_bitcoin_height,
        mainnet,
        chain_id,
        pox.clone(),
    )
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let stacks_height = targets.stacks_height;
    let stacks_tip = targets.stacks_tip.clone();
    let stacks_tip_boundary = sortition_tip_copy_boundary(&targets);
    let sortition_marf_height = targets.sortition_marf_height;
    let sortition_tip = targets.sortition_canonical_tip.clone();
    let first_bitcoin_height = targets.first_bitcoin_height;
    let squash_bitcoin_height = targets.squash_bitcoin_height;
    let stacks_boundary = SquashBoundary {
        marf_height: stacks_height,
        bitcoin_height: squash_bitcoin_height,
    };
    let sortition_boundary = SquashBoundary {
        marf_height: sortition_marf_height,
        bitcoin_height: squash_bitcoin_height,
    };

    eprintln!(
        "Squash at tenure start Bitcoin height {tenure_start_bitcoin_height}, \
         Stacks tenure-start anchor height {stacks_height}, anchor tip {stacks_tip} \
         (squash Bitcoin height {squash_bitcoin_height}, sortition MARF height {sortition_marf_height})"
    );

    // Prepare-phase warning.
    warn_if_in_prepare_phase(tenure_start_bitcoin_height, &pox, first_bitcoin_height);

    let mut clarity_out = None;
    let mut index_out = None;
    let mut sortition_out = None;

    // Phase 1: Squash & Copy

    if do_clarity {
        let out = target_out_paths(&out_root, &paths.clarity.db);
        squash_and_copy_one(
            "clarity",
            &paths.clarity,
            &out,
            &stacks_tip,
            stacks_boundary,
            SideTableMode::Clarity,
            squash_marf_open_opts(),
        );
        clarity_out = Some(out);
    }

    if do_index {
        let out = target_out_paths(&out_root, &paths.index.db);
        squash_and_copy_one(
            "index",
            &paths.index,
            &out,
            &stacks_tip,
            stacks_boundary,
            SideTableMode::Index {
                first_bitcoin_height,
                reward_cycle_len: pox.reward_cycle_length,
            },
            squash_marf_open_opts(),
        );
        index_out = Some(out);
    }

    if do_sortition {
        let out = target_out_paths_sortition(&out_root, &paths.sortition.db);
        squash_and_copy_one(
            "sortition",
            &paths.sortition,
            &out,
            &sortition_tip,
            sortition_boundary,
            SideTableMode::Sortition {
                stacks_tip_boundary: stacks_tip_boundary.clone(),
            },
            sortition_open_opts_for_path(&paths.sortition.db),
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
    let dst_blocks_dir = out_root.join("chainstate/blocks");
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
                    st.streams_copied,
                    st.streams_skipped,
                    st.microblock_rows_copied,
                    st.microblock_bytes_copied
                );
                st
            }
            Err(e) => die_with_cleanup(
                &format!("Failed to copy microblock streams: {e:?}"),
                &[&i_out.db],
            ),
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
                    "Epoch 2.x block files copied: files={}, bytes={}, genesis_skipped={}",
                    st.files_copied, st.total_bytes, st.genesis_skipped
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
        let source_canonical_tip_hex = targets.source_canonical_stacks_tip.to_hex();
        let nak_stats = match copy_nakamoto_staging_blocks(
            src_nakamoto.to_str().unwrap(),
            dst_nakamoto.to_str().unwrap(),
            dst_index_path,
            Some(source_canonical_tip_hex.as_str()),
        ) {
            Ok(st) => {
                println!(
                    "Nakamoto blocks copied: rows={}, blob_bytes={}",
                    st.rows_copied, st.total_blob_bytes
                );
                st
            }
            Err(e) => die_with_cleanup(
                &format!("Failed to copy nakamoto staging blocks: {e:?}"),
                &[dst_nakamoto.as_path(), &i_out.db],
            ),
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
    let dst_bc_db = out_root.join("burnchain/burnchain.sqlite");
    let squashed_sort = out_root.join("burnchain/sortition/marf.sqlite");
    let src_hdr = args.chainstate.join("headers.sqlite");
    let dst_hdr = out_root.join("headers.sqlite");

    if do_bitcoin_aux {
        copy_bitcoin_aux_files(
            &src_bc_db,
            &dst_bc_db,
            &squashed_sort,
            &src_hdr,
            &dst_hdr,
            squash_bitcoin_height,
        );
    }

    // Phase 2: Validation

    let mut all_valid = true;

    if !args.skip_validate {
        println!("--- Validation phase ---");

        if do_clarity
            && !validate_one(
                &paths.clarity,
                clarity_out.as_ref().unwrap(),
                &stacks_tip,
                stacks_boundary,
                args.full,
                SideTableMode::Clarity,
                squash_marf_open_opts(),
            )
        {
            all_valid = false;
        }

        if do_index
            && !validate_one(
                &paths.index,
                index_out.as_ref().unwrap(),
                &stacks_tip,
                stacks_boundary,
                args.full,
                SideTableMode::Index {
                    first_bitcoin_height,
                    reward_cycle_len: pox.reward_cycle_length,
                },
                squash_marf_open_opts(),
            )
        {
            all_valid = false;
        }

        if do_sortition
            && !validate_one(
                &paths.sortition,
                &sortition_out.as_ref().unwrap().0,
                &sortition_tip,
                sortition_boundary,
                args.full,
                SideTableMode::Sortition {
                    stacks_tip_boundary: stacks_tip_boundary.clone(),
                },
                sortition_open_opts_for_path(&paths.sortition.db),
            )
        {
            all_valid = false;
        }

        if do_blocks {
            let i_out = index_out
                .as_ref()
                .expect("--blocks requires --index; index_out must be set");
            let source_canonical_tip_hex = targets.source_canonical_stacks_tip.to_hex();
            if !validate_block_data(
                paths.index.db.to_str().unwrap(),
                i_out.db.to_str().unwrap(),
                &src_blocks_dir,
                &dst_blocks_dir,
                &src_nakamoto,
                &dst_nakamoto,
                Some(source_canonical_tip_hex.as_str()),
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
                squash_bitcoin_height,
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
            &out_root,
            clarity_out.as_ref().unwrap(),
            index_out.as_ref().unwrap(),
            (&sort_paths, sort_height),
            stacks_height,
            squash_bitcoin_height,
            blocks_stats.unwrap(),
            &copied_block_rel_paths,
        );
    }

    eprintln!(
        "Squash complete. Output: {}\n  \
         - Boot: set the node's working_dir = \"{}\"\n  \
         - Re-validate/verify: pass this output path as --squashed-chainstate / --gss-dir",
        out_root.display(),
        args.out_dir.display()
    );
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

    let tenure_start_bitcoin_height = args.tenure_start_bitcoin_height;

    // Read config and PoX constants the same way as run_squash.
    let (mainnet, chain_id) = read_db_config(&source_paths.index.db);
    let pox = build_pox_constants(mainnet, args.config.as_deref());

    // A squashed snapshot is only usable from epoch 3.4 onwards.
    enforce_minimum_tenure_height(tenure_start_bitcoin_height, mainnet, args.config.as_deref());

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

    let targets = resolve_canonical_squash_targets(
        chainstate_root.to_str().unwrap(),
        sortition_db_dir.to_str().unwrap(),
        tenure_start_bitcoin_height,
        mainnet,
        chain_id,
        pox.clone(),
    )
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let stacks_height = targets.stacks_height;
    let stacks_tip = targets.stacks_tip.clone();
    let stacks_tip_boundary = sortition_tip_copy_boundary(&targets);
    let sortition_marf_height = targets.sortition_marf_height;
    let sortition_tip = targets.sortition_canonical_tip.clone();
    let first_bitcoin_height = targets.first_bitcoin_height;
    let squash_bitcoin_height = targets.squash_bitcoin_height;
    let stacks_boundary = SquashBoundary {
        marf_height: stacks_height,
        bitcoin_height: squash_bitcoin_height,
    };
    let sortition_boundary = SquashBoundary {
        marf_height: sortition_marf_height,
        bitcoin_height: squash_bitcoin_height,
    };

    let mut all_valid = true;

    if do_clarity
        && !validate_one(
            &source_paths.clarity,
            &squashed_paths.clarity,
            &stacks_tip,
            stacks_boundary,
            args.full,
            SideTableMode::Clarity,
            squash_marf_open_opts(),
        )
    {
        all_valid = false;
    }

    if do_index
        && !validate_one(
            &source_paths.index,
            &squashed_paths.index,
            &stacks_tip,
            stacks_boundary,
            args.full,
            SideTableMode::Index {
                first_bitcoin_height,
                reward_cycle_len: pox.reward_cycle_length,
            },
            squash_marf_open_opts(),
        )
    {
        all_valid = false;
    }

    if do_sortition
        && !validate_one(
            &source_paths.sortition,
            &squashed_paths.sortition,
            &sortition_tip,
            sortition_boundary,
            args.full,
            SideTableMode::Sortition {
                stacks_tip_boundary: stacks_tip_boundary.clone(),
            },
            sortition_open_opts_for_path(&source_paths.sortition.db),
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
        let source_canonical_tip_hex = targets.source_canonical_stacks_tip.to_hex();
        if !validate_block_data(
            source_paths.index.db.to_str().unwrap(),
            squashed_paths.index.db.to_str().unwrap(),
            &src_blocks_dir,
            &dst_blocks_dir,
            &src_nakamoto,
            &dst_nakamoto,
            Some(source_canonical_tip_hex.as_str()),
        ) {
            all_valid = false;
        }
    }

    // Bitcoin auxiliary validation.
    let do_bitcoin_aux = args.bitcoin || args.all;
    ensure_flag_requires("bitcoin", do_bitcoin_aux, "sortition", do_sortition);
    if do_bitcoin_aux {
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
            squash_bitcoin_height,
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{is_known_network, network_subdir};
    use crate::util::target_out_paths;

    #[test]
    fn network_subdir_mainnet_is_forced() {
        // Mainnet always boots under `mainnet/`, regardless of the source path.
        assert_eq!(
            network_subdir(true, Path::new("/stacks/anything")).unwrap(),
            "mainnet"
        );
        assert_eq!(network_subdir(true, Path::new("/")).unwrap(), "mainnet");
    }

    #[test]
    fn network_subdir_mirrors_testnet_path() {
        assert_eq!(
            network_subdir(false, Path::new("/stacks/krypton")).unwrap(),
            "krypton"
        );
        // Trailing slash is stripped.
        assert_eq!(
            network_subdir(false, Path::new("/stacks/krypton/")).unwrap(),
            "krypton"
        );
    }

    #[test]
    fn network_subdir_rejects_undeterminable_path() {
        assert!(network_subdir(false, Path::new("/")).is_err());
        assert!(network_subdir(false, Path::new("..")).is_err());
    }

    #[test]
    fn network_subdir_rejects_mainnet_dir_for_non_mainnet() {
        // A testnet/regtest chainstate can never boot as mainnet.
        assert!(network_subdir(false, Path::new("/stacks/mainnet")).is_err());
    }

    #[test]
    fn network_subdir_unknown_errors() {
        assert!(network_subdir(false, Path::new("/stacks/weirdnet")).is_err());
    }

    #[test]
    fn known_networks() {
        assert!(is_known_network("krypton"));
        assert!(is_known_network("mainnet"));
        assert!(!is_known_network("weirdnet"));
    }

    #[test]
    fn output_lands_under_network_subdir() {
        // The layout contract: `--out-dir` is the working_dir, and the squash
        // targets land at `<out-dir>/<network>/...`.
        let out_dir = Path::new("/tmp/out");

        // Testnet: the source subdir is mirrored.
        let subdir = network_subdir(false, Path::new("/data/krypton")).unwrap();
        let out_root = out_dir.join(subdir);
        let tp = target_out_paths(
            &out_root,
            Path::new("/data/krypton/chainstate/vm/clarity/marf.sqlite"),
        );
        assert_eq!(
            tp.db.to_str().unwrap(),
            "/tmp/out/krypton/chainstate/vm/clarity/marf.sqlite"
        );

        // Mainnet is forced into `mainnet/` regardless of the source path.
        let mainnet_root = out_dir.join(network_subdir(true, Path::new("/data/x")).unwrap());
        let tp_m = target_out_paths(
            &mainnet_root,
            Path::new("/data/x/chainstate/vm/index.sqlite"),
        );
        assert_eq!(
            tp_m.db.to_str().unwrap(),
            "/tmp/out/mainnet/chainstate/vm/index.sqlite"
        );
    }
}
