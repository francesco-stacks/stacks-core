use std::collections::HashSet;
use std::fs;
use std::path::Path;

use stacks_common::types::chainstate::{SortitionId, StacksBlockId};
use stackslib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
use stackslib::chainstate::stacks::index::{MarfTrieId, trie_sql};

use crate::cli::{
    BlocksSection, ChecksumsSection, GSS_MANIFEST, RootsSection, SnapshotSection, SquashManifest,
    SquashRootsSection, TargetPaths,
};
use crate::util::{
    compute_aggregate_checksum, compute_checksums, derive_expected_epoch2_block_rel_paths,
    format_timestamp, sortition_open_opts_for_path, squash_marf_open_opts,
};

/// Squash metadata read from a just-squashed MARF DB.
pub struct ReadSquashMetadata<T: MarfTrieId> {
    pub tip: T,
    pub archival_root_hash: String,
    pub squash_root_node_hash: String,
    pub squash_height: u32,
}

/// Read squash metadata from a just-squashed MARF DB.
pub fn read_squash_metadata<T: MarfTrieId + std::fmt::Display>(
    db_path: &str,
    open_opts: MARFOpenOpts,
) -> ReadSquashMetadata<T> {
    let marf = MARF::<T>::from_path(db_path, open_opts).unwrap_or_else(|e| {
        eprintln!("Failed to open squashed MARF for manifest: {e:?}");
        std::process::exit(1);
    });
    let tip = trie_sql::get_latest_confirmed_block_hash(marf.sqlite_conn()).unwrap_or_else(|e| {
        eprintln!("Failed to read latest block hash: {e:?}");
        std::process::exit(1);
    });
    let info = trie_sql::read_squash_info(marf.sqlite_conn())
        .unwrap_or_else(|e| {
            eprintln!("Failed to read squash info: {e:?}");
            std::process::exit(1);
        })
        .unwrap_or_else(|| {
            eprintln!("No squash info found in DB");
            std::process::exit(1);
        });
    ReadSquashMetadata {
        tip,
        archival_root_hash: format!("0x{}", info.archival_marf_root_hash),
        squash_root_node_hash: format!("0x{}", info.squash_root_node_hash),
        squash_height: info.squash_height,
    }
}

/// Insert the relative path of `abs_path` (relative to `base`) into `set`.
fn insert_expected_rel(base: &Path, abs_path: &Path, set: &mut HashSet<String>) {
    if let Ok(rel) = abs_path.strip_prefix(base) {
        set.insert(rel.to_string_lossy().replace('\\', "/"));
    }
}

/// Assert that a squashed DB stores the squash height the caller expected.
/// Exits on mismatch.
fn assert_squash_height(label: &str, actual: u32, expected: u32) {
    if actual != expected {
        eprintln!("Manifest error: {label} squash MARF height {actual} != expected {expected}");
        std::process::exit(1);
    }
}

/// Read the burn_header_timestamp for the snapshot at the squash height
/// from the squashed sortition DB. Exits on failure.
pub fn read_snapshot_timestamp(sortition_out: &TargetPaths, sortition_marf_height: u32) -> String {
    let conn = rusqlite::Connection::open(sortition_out.db.to_str().unwrap()).unwrap_or_else(|e| {
        eprintln!("Failed to open squashed sortition DB for snapshot timestamp: {e}");
        std::process::exit(1);
    });
    let sort_id: String = conn
        .query_row(
            "SELECT lower(hex(block_hash)) FROM marf_squashed_blocks WHERE height = ?1",
            [sortition_marf_height],
            |row| row.get(0),
        )
        .unwrap_or_else(|e| {
            eprintln!(
                "Failed to read sortition ID at sortition MARF height {sortition_marf_height} from squashed sortition DB: {e}"
            );
            std::process::exit(1);
        });
    let ts: i64 = conn
        .query_row(
            "SELECT burn_header_timestamp FROM snapshots WHERE sortition_id = ?1",
            [&sort_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|e| {
            eprintln!("Failed to read burn_header_timestamp for sortition_id {sort_id}: {e}");
            std::process::exit(1);
        });
    format_timestamp(ts)
}

/// Generate the GSS manifest. Only called for a complete GSS (all MARFs +
/// blocks + bitcoin aux).
#[allow(clippy::too_many_arguments)]
pub fn generate_manifest(
    out_dir: &Path,
    clarity_out: &TargetPaths,
    index_out: &TargetPaths,
    sortition_out: (&TargetPaths, u32),
    stacks_height: u32,
    bitcoin_height: u32,
    blocks_section: BlocksSection,
) {
    let index_meta = read_squash_metadata::<StacksBlockId>(
        index_out.db.to_str().unwrap(),
        squash_marf_open_opts(),
    );
    assert_squash_height("Index", index_meta.squash_height, stacks_height);

    let clarity_meta = read_squash_metadata::<StacksBlockId>(
        clarity_out.db.to_str().unwrap(),
        squash_marf_open_opts(),
    );
    assert_squash_height("Clarity", clarity_meta.squash_height, stacks_height);
    if clarity_meta.tip != index_meta.tip {
        eprintln!(
            "Manifest error: Clarity tip {} != Index tip {}",
            clarity_meta.tip, index_meta.tip
        );
        std::process::exit(1);
    }

    let (sortition_paths, sortition_marf_height) = sortition_out;
    let sortition_meta = read_squash_metadata::<SortitionId>(
        sortition_paths.db.to_str().unwrap(),
        sortition_open_opts_for_path(&sortition_paths.db),
    );
    assert_squash_height(
        "Sortition",
        sortition_meta.squash_height,
        sortition_marf_height,
    );

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

    // Read timestamp from squashed sortition snapshots.
    let timestamp = Some(read_snapshot_timestamp(
        sortition_paths,
        sortition_marf_height,
    ));

    // Read bitcoin block hash from sortition DB.
    let bitcoin_block_hash = {
        let conn =
            rusqlite::Connection::open(sortition_paths.db.to_str().unwrap()).unwrap_or_else(|e| {
                eprintln!("Failed to open squashed sortition DB for bitcoin metadata: {e}");
                std::process::exit(1);
            });
        let sort_id: String = conn
            .query_row(
                "SELECT lower(hex(block_hash)) FROM marf_squashed_blocks WHERE height = ?1",
                [sortition_marf_height],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| {
                eprintln!(
                    "Failed to read sortition ID at sortition MARF height {sortition_marf_height} from squashed sortition DB: {e}"
                );
                std::process::exit(1);
            });
        let (btc_hash, snapshot_burn_height): (String, i64) = conn
            .query_row(
                "SELECT burn_header_hash, block_height FROM snapshots WHERE sortition_id = ?1",
                [&sort_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or_else(|e| {
                eprintln!("Failed to read snapshot for sortition_id {sort_id}: {e}");
                std::process::exit(1);
            });
        // The boundary sortition (selected by `sortition_marf_height`) must sit at
        // the Bitcoin height recorded in the manifest; a mismatch means the
        // snapshot's `bitcoin_height` disagrees with the sortition MARF it squashed.
        if snapshot_burn_height != i64::from(bitcoin_height) {
            eprintln!(
                "Manifest error: boundary sortition Bitcoin height {snapshot_burn_height} != manifest bitcoin_height {bitcoin_height}"
            );
            std::process::exit(1);
        }
        format!("0x{btc_hash}")
    };

    // Build the set of individually hashed files so that stale files in a
    // reused out-dir are rejected rather than blessed into the manifest.
    let mut expected = HashSet::new();

    // MARF databases + blobs.
    insert_expected_rel(out_dir, &clarity_out.db, &mut expected);
    if let Some(b) = &clarity_out.blobs {
        insert_expected_rel(out_dir, b, &mut expected);
    }
    insert_expected_rel(out_dir, &index_out.db, &mut expected);
    if let Some(b) = &index_out.blobs {
        insert_expected_rel(out_dir, b, &mut expected);
    }
    insert_expected_rel(out_dir, &sortition_paths.db, &mut expected);
    if let Some(b) = &sortition_paths.blobs {
        insert_expected_rel(out_dir, b, &mut expected);
    }

    // Bitcoin auxiliary files.
    expected.insert("burnchain/burnchain.sqlite".to_string());
    expected.insert("headers.sqlite".to_string());

    // `nakamoto.sqlite` is hashed individually; epoch-2 block files are
    // covered by one aggregate checksum to keep the manifest compact.
    expected.insert("chainstate/blocks/nakamoto.sqlite".to_string());
    let epoch2_block_rel_paths = derive_expected_epoch2_block_rel_paths(&index_out.db)
        .unwrap_or_else(|e| {
            eprintln!("Failed to derive epoch-2 block files from index.sqlite: {e}");
            std::process::exit(1);
        });
    if epoch2_block_rel_paths.len() as u64 != blocks_section.epoch2x_files {
        eprintln!(
            "Manifest error: index lists {} epoch-2 block files, but the copy reported {}",
            epoch2_block_rel_paths.len(),
            blocks_section.epoch2x_files
        );
        std::process::exit(1);
    }

    let skipped_epoch2: HashSet<String> = epoch2_block_rel_paths.iter().cloned().collect();
    let files =
        compute_checksums(out_dir, Some(&expected), Some(&skipped_epoch2)).unwrap_or_else(|e| {
            eprintln!("Failed to compute checksums: {e}");
            std::process::exit(1);
        });
    let epoch2_block_archive_hash = compute_aggregate_checksum(out_dir, &epoch2_block_rel_paths)
        .unwrap_or_else(|e| {
            eprintln!("Failed to compute epoch-2 block archive hash: {e}");
            std::process::exit(1);
        });
    println!(
        "Computed SHA-256 checksums for {} files plus one epoch-2 block archive hash",
        files.len()
    );

    let manifest = SquashManifest {
        snapshot: SnapshotSection {
            version: 1,
            stacks_height,
            bitcoin_height,
            block_hash: format!("0x{}", index_meta.tip),
            bitcoin_block_hash: Some(bitcoin_block_hash),
            timestamp,
            chain_id,
            mainnet,
        },
        roots: RootsSection {
            clarity_archival_marf_root_hash: Some(clarity_meta.archival_root_hash),
            index_archival_marf_root_hash: index_meta.archival_root_hash,
            sortition_archival_marf_root_hash: Some(sortition_meta.archival_root_hash),
        },
        squash_roots: SquashRootsSection {
            clarity_squash_root_node_hash: Some(clarity_meta.squash_root_node_hash),
            index_squash_root_node_hash: Some(index_meta.squash_root_node_hash),
            sortition_squash_root_node_hash: Some(sortition_meta.squash_root_node_hash),
        },
        blocks: Some(blocks_section),
        checksums: Some(ChecksumsSection {
            files,
            epoch2_block_archive_hash: Some(epoch2_block_archive_hash),
        }),
    };

    let toml_str = toml::to_string(&manifest).unwrap_or_else(|e| {
        eprintln!("Failed to serialize manifest: {e}");
        std::process::exit(1);
    });

    let manifest_path = out_dir.join(GSS_MANIFEST);
    fs::write(&manifest_path, toml_str).unwrap_or_else(|e| {
        eprintln!(
            "Failed to write manifest to '{}': {e}",
            manifest_path.display()
        );
        std::process::exit(1);
    });
    println!("Manifest written to {}", manifest_path.display());
}
