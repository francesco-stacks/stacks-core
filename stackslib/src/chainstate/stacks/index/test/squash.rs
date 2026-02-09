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

use std::path::PathBuf;

use stacks_common::types::chainstate::{StacksBlockId, TrieHash};
use tempfile::tempdir;

use crate::chainstate::stacks::index::marf::{
    MARFOpenOpts, MarfConnection, SquashStats, MARF, OWN_BLOCK_HEIGHT_KEY,
};
use crate::chainstate::stacks::index::storage::TrieHashCalculationMode;
use crate::chainstate::stacks::index::{trie_sql, ClarityMarfTrieId, Error, MARFValue};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a small MARF with 2 blocks for basic squash tests.
fn setup_marf(path: &str) -> (MARF<StacksBlockId>, StacksBlockId, StacksBlockId) {
    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut marf = MARF::<StacksBlockId>::from_path(path, open_opts).unwrap();

    let b1 = StacksBlockId::from_bytes(&[1u8; 32]).unwrap();
    let b2 = StacksBlockId::from_bytes(&[2u8; 32]).unwrap();

    marf.begin(&StacksBlockId::sentinel(), &b1).unwrap();
    marf.insert("k1", MARFValue::from_value("v1")).unwrap();
    marf.commit().unwrap();

    marf.begin(&b1, &b2).unwrap();
    marf.insert("k1", MARFValue::from_value("v2")).unwrap();
    marf.insert("k2", MARFValue::from_value("v3")).unwrap();
    marf.commit().unwrap();

    (marf, b1, b2)
}

/// Create a larger MARF with 10 blocks (heights 0-9) for skip-list coverage.
///
/// k1 is updated at every block (exercises backpointers at every depth).
/// k2..k10 are each inserted at their respective blocks.
fn setup_large_marf(path: &str) -> (MARF<StacksBlockId>, Vec<StacksBlockId>) {
    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut marf = MARF::<StacksBlockId>::from_path(path, open_opts).unwrap();

    let blocks: Vec<StacksBlockId> = (1..=10u8)
        .map(|i| StacksBlockId::from_bytes(&[i; 32]).unwrap())
        .collect();

    // Block at height 0
    marf.begin(&StacksBlockId::sentinel(), &blocks[0]).unwrap();
    marf.insert("k1", MARFValue::from_value("v1_at_0")).unwrap();
    marf.commit().unwrap();

    // Heights 1-9
    for i in 1..blocks.len() {
        marf.begin(&blocks[i - 1], &blocks[i]).unwrap();
        let key = format!("k{}", i + 1);
        let val = format!("v{}_at_{}", i + 1, i);
        marf.insert(&key, MARFValue::from_value(&val)).unwrap();
        marf.insert("k1", MARFValue::from_value(&format!("v1_at_{i}")))
            .unwrap();
        marf.commit().unwrap();
    }

    (marf, blocks)
}

fn squash_helper(src_path: &str, dst_dir: &std::path::Path, height: u32) -> (PathBuf, SquashStats) {
    std::fs::create_dir_all(dst_dir).unwrap();
    let dst_db_path = dst_dir.join("index.sqlite");
    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let stats = MARF::<StacksBlockId>::squash_to_path(
        src_path,
        dst_db_path.to_str().unwrap(),
        open_opts,
        height,
    )
    .unwrap();
    (dst_db_path, stats)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_squash_to_path_outputs_data() {
    let dir = tempdir().unwrap();
    let src_db_path = dir.path().join("index.sqlite");
    let _ = setup_marf(src_db_path.to_str().unwrap());

    let (dst_db_path, stats) = squash_helper(
        src_db_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        1,
    );

    assert!(stats.leaf_count > 0);
    assert!(dst_db_path.exists());
    assert!(PathBuf::from(format!("{}.blobs", dst_db_path.display())).exists());

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut dst =
        MARF::<StacksBlockId>::from_path(dst_db_path.to_str().unwrap(), open_opts).unwrap();
    let b2 = StacksBlockId::from_bytes(&[2u8; 32]).unwrap();
    let k1 = dst.get(&b2, "k1").unwrap().unwrap();
    assert_eq!(k1, MARFValue::from_value("v2"));
    let own_height = dst.get(&b2, OWN_BLOCK_HEIGHT_KEY).unwrap().unwrap();
    assert_eq!(own_height, MARFValue::from(1u32));
}

#[test]
fn test_squash_info_detected_on_open() {
    let dir = tempdir().unwrap();
    let src_db_path = dir.path().join("index.sqlite");
    let _ = setup_marf(src_db_path.to_str().unwrap());

    let (dst_db_path, _) = squash_helper(
        src_db_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        1,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut squashed =
        MARF::<StacksBlockId>::from_path(dst_db_path.to_str().unwrap(), open_opts).unwrap();
    let tip =
        trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(squashed.sqlite_conn()).unwrap();

    // Verify squash metadata was detected from the SQL table on open.
    let (is_squashed, info_root, info_height, info_block) = squashed
        .with_conn(
            |conn| -> Result<(bool, TrieHash, u32, StacksBlockId), Error> {
                let info = conn.squash_info().expect("missing squash info");
                Ok((
                    conn.is_squashed(),
                    info.root,
                    info.height,
                    info.block_hash.clone(),
                ))
            },
        )
        .unwrap();

    // Cross-check with the SQL table directly.
    let (sql_root, sql_height) = trie_sql::read_squash_info(squashed.sqlite_conn())
        .unwrap()
        .expect("SQL squash info missing");

    assert!(is_squashed);
    assert_eq!(info_root, sql_root);
    assert_eq!(info_height, sql_height);
    assert_eq!(info_height, 1);
    assert_eq!(info_block, tip);
}

#[test]
fn test_squash_info_absent_on_archival_open() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("index.sqlite");
    let (mut marf, _b1, _b2) = setup_marf(db_path.to_str().unwrap());

    let (is_squashed, has_info) = marf
        .with_conn(|conn| -> Result<(bool, bool), Error> {
            Ok((conn.is_squashed(), conn.squash_info().is_some()))
        })
        .unwrap();

    assert!(!is_squashed);
    assert!(!has_info);
}

#[test]
fn test_squashed_marf_can_extend_past_snapshot_height() {
    let dir = tempdir().unwrap();
    let src_db_path = dir.path().join("index.sqlite");
    let _ = setup_marf(src_db_path.to_str().unwrap());

    let (dst_db_path, _) = squash_helper(
        src_db_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        1,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut squashed =
        MARF::<StacksBlockId>::from_path(dst_db_path.to_str().unwrap(), open_opts).unwrap();

    let b2 = StacksBlockId::from_bytes(&[2u8; 32]).unwrap();
    let b3 = StacksBlockId::from_bytes(&[3u8; 32]).unwrap();
    let b4 = StacksBlockId::from_bytes(&[4u8; 32]).unwrap();

    squashed.begin(&b2, &b3).unwrap();
    squashed.insert("k3", MARFValue::from_value("v4")).unwrap();
    squashed.commit().unwrap();

    squashed.begin(&b3, &b4).unwrap();
    squashed.insert("k4", MARFValue::from_value("v5")).unwrap();
    squashed.commit().unwrap();

    let v4 = squashed.get(&b4, "k4").unwrap().unwrap();
    assert_eq!(v4, MARFValue::from_value("v5"));
    let own_height = squashed.get(&b4, OWN_BLOCK_HEIGHT_KEY).unwrap().unwrap();
    assert_eq!(own_height, MARFValue::from(3u32));
}

#[test]
fn test_validate_squashed_correct_fast() {
    let dir = tempdir().unwrap();
    let src_db_path = dir.path().join("index.sqlite");
    let _ = setup_marf(src_db_path.to_str().unwrap());

    let (dst_db_path, _) = squash_helper(
        src_db_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        1,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    // Fast path (default) - no leaf scan.
    let stats = MARF::<StacksBlockId>::validate_squashed_at_height(
        src_db_path.to_str().unwrap(),
        dst_db_path.to_str().unwrap(),
        open_opts,
        1,
    )
    .unwrap();

    assert!(stats.squash_root_present, "squash root present");
    assert!(stats.squash_root_matches, "squash root matches");
    assert_eq!(stats.root_hash_missing, 0);
    assert_eq!(stats.root_hash_mismatches, 0);
    assert_eq!(stats.blob_offset_mismatches, 0);
    // Fast path skips leaf scan - these should be 0.
    assert_eq!(stats.source_keys_checked, 0);
    assert_eq!(stats.squashed_keys_checked, 0);
}

#[test]
fn test_validate_squashed_correct_full() {
    let dir = tempdir().unwrap();
    let src_db_path = dir.path().join("index.sqlite");
    let _ = setup_marf(src_db_path.to_str().unwrap());

    let (dst_db_path, _) = squash_helper(
        src_db_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        1,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    // Full leaf scan mode.
    let stats = MARF::<StacksBlockId>::validate_squashed_at_height_ex(
        src_db_path.to_str().unwrap(),
        dst_db_path.to_str().unwrap(),
        open_opts,
        1,
        true,
    )
    .unwrap();

    assert!(stats.squash_root_present, "squash root present");
    assert!(stats.squash_root_matches, "squash root matches");
    assert_eq!(stats.root_hash_missing, 0);
    assert_eq!(stats.root_hash_mismatches, 0);
    assert_eq!(stats.blob_offset_mismatches, 0);
    // Full scan populates leaf counts.
    assert_eq!(stats.missing_in_squashed, 0, "missing in squashed");
    assert_eq!(stats.missing_in_source, 0, "missing in source");
    assert_eq!(stats.value_mismatches, 0, "value mismatches");
    assert!(stats.source_keys_checked > 0);
    assert!(stats.squashed_keys_checked > 0);
}

#[test]
fn test_validate_detects_wrong_height() {
    // Squash at height 1 but validate at height 0.
    // Source state at height 0 differs from squashed state at height 1
    // (k1 was overwritten between blocks), so validation must report
    // a root hash mismatch.
    let dir = tempdir().unwrap();
    let src_db_path = dir.path().join("index.sqlite");
    let _ = setup_marf(src_db_path.to_str().unwrap());

    let (dst_db_path, _) = squash_helper(
        src_db_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        1,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let stats = MARF::<StacksBlockId>::validate_squashed_at_height(
        src_db_path.to_str().unwrap(),
        dst_db_path.to_str().unwrap(),
        open_opts,
        0,
    )
    .unwrap();

    // Source at height 0 has a different root hash than squashed (at height 1).
    assert!(
        !stats.squash_root_matches,
        "Expected squash root mismatch when validating at wrong height: {stats:?}"
    );
}

#[test]
fn test_large_marf_squash_extend_root_hash_matches_archival() {
    // Squash a 10-block MARF at height 8, then extend both the archival
    // and squashed MARFs with the same data at heights 9 and 10.
    let dir = tempdir().unwrap();
    let archival_path = dir.path().join("archival.sqlite");
    let (mut archival, blocks) = setup_large_marf(archival_path.to_str().unwrap());

    let (squashed_path, _) = squash_helper(
        archival_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        8,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut squashed =
        MARF::<StacksBlockId>::from_path(squashed_path.to_str().unwrap(), open_opts).unwrap();

    let b_new_9 = StacksBlockId::from_bytes(&[101u8; 32]).unwrap();
    let b_new_10 = StacksBlockId::from_bytes(&[102u8; 32]).unwrap();

    // --- Extend archival ---
    archival.begin(&blocks[8], &b_new_9).unwrap();
    archival
        .insert("k_new_9", MARFValue::from_value("val9"))
        .unwrap();
    archival.commit().unwrap();

    archival.begin(&b_new_9, &b_new_10).unwrap();
    archival
        .insert("k_new_10", MARFValue::from_value("val10"))
        .unwrap();
    archival.commit().unwrap();

    // --- Extend squashed ---
    squashed.begin(&blocks[8], &b_new_9).unwrap();
    squashed
        .insert("k_new_9", MARFValue::from_value("val9"))
        .unwrap();
    squashed.commit().unwrap();

    squashed.begin(&b_new_9, &b_new_10).unwrap();
    squashed
        .insert("k_new_10", MARFValue::from_value("val10"))
        .unwrap();
    squashed.commit().unwrap();

    // (a) Data inserted at the extended heights is readable.
    assert_eq!(
        squashed.get(&b_new_9, "k_new_9").unwrap().unwrap(),
        MARFValue::from_value("val9")
    );
    assert_eq!(
        squashed.get(&b_new_10, "k_new_10").unwrap().unwrap(),
        MARFValue::from_value("val10")
    );
    assert_eq!(
        squashed.get(&b_new_10, "k1").unwrap().unwrap(),
        MARFValue::from_value("v1_at_8")
    );

    // (b) MARF root hashes at the extended heights must match.
    let archival_root_9 = archival.get_root_hash_at(&b_new_9).unwrap();
    let squashed_root_9 = squashed.get_root_hash_at(&b_new_9).unwrap();
    assert_eq!(
        archival_root_9, squashed_root_9,
        "Root hash mismatch at height 9"
    );

    let archival_root_10 = archival.get_root_hash_at(&b_new_10).unwrap();
    let squashed_root_10 = squashed.get_root_hash_at(&b_new_10).unwrap();
    assert_eq!(
        archival_root_10, squashed_root_10,
        "Root hash mismatch at height 10"
    );

    assert_ne!(archival_root_9, TrieHash([0u8; 32]), "root at 9 is zero");
    assert_ne!(archival_root_10, TrieHash([0u8; 32]), "root at 10 is zero");
    assert_ne!(
        archival_root_9, archival_root_10,
        "roots at 9 and 10 should differ"
    );

    let own_h = squashed
        .get(&b_new_10, OWN_BLOCK_HEIGHT_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(own_h, MARFValue::from(10u32));
}

/// Squash at height 5, then extend both MARFs through 10 additional
/// heights and verify hash equality at EVERY extended height.
#[test]
fn test_multi_height_extension_hash_equality() {
    let dir = tempdir().unwrap();
    let archival_path = dir.path().join("archival.sqlite");
    let (mut archival, blocks) = setup_large_marf(archival_path.to_str().unwrap());

    let (squashed_path, _) = squash_helper(
        archival_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        5,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut squashed =
        MARF::<StacksBlockId>::from_path(squashed_path.to_str().unwrap(), open_opts).unwrap();

    let mut prev_block = blocks[5].clone();
    let mut new_blocks: Vec<StacksBlockId> = Vec::new();
    for i in 0..10u8 {
        let new_bh = StacksBlockId::from_bytes(&[200 + i; 32]).unwrap();
        let key = format!("ext_k{i}");
        let val = format!("ext_v{i}");

        archival.begin(&prev_block, &new_bh).unwrap();
        archival.insert(&key, MARFValue::from_value(&val)).unwrap();
        archival.commit().unwrap();

        squashed.begin(&prev_block, &new_bh).unwrap();
        squashed.insert(&key, MARFValue::from_value(&val)).unwrap();
        squashed.commit().unwrap();

        new_blocks.push(new_bh.clone());
        prev_block = new_bh;
    }

    for (i, bh) in new_blocks.iter().enumerate() {
        let arch_root = archival.get_root_hash_at(bh).unwrap();
        let sq_root = squashed.get_root_hash_at(bh).unwrap();
        assert_eq!(
            arch_root,
            sq_root,
            "Root hash mismatch at extended height {}",
            i + 6
        );
        assert_ne!(arch_root, TrieHash([0u8; 32]), "root at {} is zero", i + 6);
    }

    let last = new_blocks.last().unwrap();
    assert_eq!(
        squashed.get(last, "k1").unwrap().unwrap(),
        MARFValue::from_value("v1_at_5"),
    );
    assert_eq!(
        squashed.get(last, "ext_k9").unwrap().unwrap(),
        MARFValue::from_value("ext_v9"),
    );
}

/// Verify that all historical marf_data entries share the same
/// external_offset (i.e. point to the single shared blob).
#[test]
fn test_marf_data_entries_share_blob_offset() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("index.sqlite");
    let (_, blocks) = setup_large_marf(src_path.to_str().unwrap());

    let (dst_path, _) = squash_helper(src_path.to_str().unwrap(), &dir.path().join("squashed"), 8);

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let squashed = MARF::<StacksBlockId>::from_path(dst_path.to_str().unwrap(), open_opts).unwrap();
    let conn = squashed.sqlite_conn();

    let tip_id = trie_sql::get_block_identifier(conn, &blocks[8]).unwrap();
    let (tip_offset, tip_length) = trie_sql::get_external_trie_offset_length(conn, tip_id).unwrap();
    assert!(tip_length > 0, "blob length should be non-zero");

    for i in 0..8 {
        let blk_id = trie_sql::get_block_identifier(conn, &blocks[i]).unwrap();
        let (offset, length) = trie_sql::get_external_trie_offset_length(conn, blk_id).unwrap();
        assert_eq!(offset, tip_offset, "block {i} offset mismatch");
        assert_eq!(length, tip_length, "block {i} length mismatch");
    }
}

/// Verify that walk_cow correctly follows annotated back_block values
/// when copying nodes from a squashed blob into a new block.
#[test]
fn test_walk_cow_preserves_backpointer_identity() {
    let dir = tempdir().unwrap();
    let archival_path = dir.path().join("archival.sqlite");
    let (mut archival, blocks) = setup_large_marf(archival_path.to_str().unwrap());

    let (squashed_path, _) = squash_helper(
        archival_path.to_str().unwrap(),
        &dir.path().join("squashed"),
        9,
    );

    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
    let mut squashed =
        MARF::<StacksBlockId>::from_path(squashed_path.to_str().unwrap(), open_opts).unwrap();

    let b_new = StacksBlockId::from_bytes(&[250u8; 32]).unwrap();
    squashed.begin(&blocks[9], &b_new).unwrap();
    squashed
        .insert("k1", MARFValue::from_value("v1_at_10"))
        .unwrap();
    squashed
        .insert("new_key", MARFValue::from_value("new_val"))
        .unwrap();
    squashed.commit().unwrap();

    for i in 2..=10 {
        let key = format!("k{i}");
        let result = squashed.get(&b_new, &key).unwrap();
        assert!(result.is_some(), "missing key {key} after extend");
    }

    assert_eq!(
        squashed.get(&b_new, "k1").unwrap().unwrap(),
        MARFValue::from_value("v1_at_10"),
    );

    assert_eq!(
        squashed.get(&b_new, "new_key").unwrap().unwrap(),
        MARFValue::from_value("new_val"),
    );

    archival.begin(&blocks[9], &b_new).unwrap();
    archival
        .insert("k1", MARFValue::from_value("v1_at_10"))
        .unwrap();
    archival
        .insert("new_key", MARFValue::from_value("new_val"))
        .unwrap();
    archival.commit().unwrap();

    let arch_root = archival.get_root_hash_at(&b_new).unwrap();
    let sq_root = squashed.get_root_hash_at(&b_new).unwrap();
    assert_eq!(arch_root, sq_root, "Root hash mismatch after walk_cow");
}
