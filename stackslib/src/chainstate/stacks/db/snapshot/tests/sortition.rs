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

//! Sortition side-table copy/validate tests.

use rusqlite::{params, Connection};
use stacks_common::types::chainstate::{BlockHeaderHash, ConsensusHash, SortitionId};
use tempfile::tempdir;

use super::super::common::{unclassified_tables, MARF_INFRA_TABLES};
use super::super::sortition::{
    copy_sortition_side_tables, copy_sortition_side_tables_with_boundary,
    validate_sortition_side_tables, validate_sortition_side_tables_with_boundary,
    SortitionTipCopyBoundary,
};
use super::{hex_id, label_block_id};
use crate::chainstate::burn::db::sortdb::{
    SORTITION_DB_INITIAL_SCHEMA, SORTITION_DB_SCHEMA_10, SORTITION_DB_SCHEMA_11,
    SORTITION_DB_SCHEMA_2, SORTITION_DB_SCHEMA_3, SORTITION_DB_SCHEMA_4, SORTITION_DB_SCHEMA_5,
    SORTITION_DB_SCHEMA_6, SORTITION_DB_SCHEMA_7, SORTITION_DB_SCHEMA_8, SORTITION_DB_SCHEMA_9,
    SORTITION_DB_VERSION,
};
use crate::chainstate::stacks::index::marf::{MARFOpenOpts, MARF};
use crate::chainstate::stacks::index::{ClarityMarfTrieId, Error, MARFValue};

/// Create a sortition source DB with the real schema (all migrations
/// through schema 11). Applies only the DDL; epoch data inserts are
/// skipped since tests only need the table structure.
fn create_sortition_source_db(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    for cmd in SORTITION_DB_INITIAL_SCHEMA {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_2 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_3 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_4 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_5 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_6 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_7 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_8 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_9 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_10 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SORTITION_DB_SCHEMA_11 {
        conn.execute_batch(cmd).unwrap();
    }
    // Sentinel: a new sortition schema bumps SORTITION_DB_VERSION; this
    // fixture must then replay the new migration too.
    assert_eq!(
        SORTITION_DB_VERSION, 11,
        "sortition schema changed: replay the new migration in this fixture"
    );
    conn.execute(
        "INSERT OR REPLACE INTO db_config (version) VALUES (?1)",
        params![SORTITION_DB_VERSION.to_string()],
    )
    .unwrap();
    // Same reason as `create_source_db`: satisfy the strict
    // src-has-table check in `copy_canonical_fork_storage`.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS __fork_storage (
            value_hash TEXT NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY(value_hash)
        );",
    )
    .unwrap();
    conn
}

/// Insert a snapshot row for the given sortition_id and burn_header_hash labels.
///
/// `sortition_id` is stored as its [`hex_id`] form so it joins against the
/// canonical-sortitions temp table, which `populate_canonical_sortitions`
/// fills with the lowercase-hex ids read via
/// `trie_sql::bulk_read_squashed_blocks`.
fn insert_snapshot(
    conn: &Connection,
    sortition_id_label: &str,
    burn_header_hash: &str,
    block_height: u32,
) {
    conn.execute(
        "INSERT INTO snapshots (
                block_height, burn_header_hash, sortition_id, parent_sortition_id,
                burn_header_timestamp, parent_burn_header_hash, consensus_hash,
                ops_hash, total_burn, sortition, sortition_hash,
                winning_block_txid, winning_stacks_block_hash, index_root,
                num_sortitions, stacks_block_accepted, stacks_block_height,
                arrival_index, canonical_stacks_tip_height, canonical_stacks_tip_hash,
                canonical_stacks_tip_consensus_hash, pox_valid,
                accumulated_coinbase_ustx, pox_payouts, miner_pk_hash
            ) VALUES (
                ?1, ?2, ?3, 'parent_sort', 1000, 'parent_bhh', ?4,
                'ops', '0', 1, 'shash', 'wbtxid', 'wsbh', ?5,
                ?1, 0, 0, ?1, 0, 'csth', 'cstch', 1, '0', '[]', NULL
            )",
        params![
            block_height,
            burn_header_hash,
            hex_id(sortition_id_label),
            format!("ch_{sortition_id_label}"),
            format!("ir_{sortition_id_label}"),
        ],
    )
    .unwrap();
}

/// Insert a leader_keys row for the given sortition_id label.
fn insert_leader_key(conn: &Connection, sortition_id_label: &str) {
    conn.execute(
        "INSERT INTO leader_keys (txid, vtxindex, block_height, burn_header_hash, \
             sortition_id, consensus_hash, public_key, memo) \
             VALUES (?1, 0, 1, 'bhh', ?2, 'ch', 'pk', 'memo')",
        params![
            format!("lk_tx_{sortition_id_label}"),
            hex_id(sortition_id_label),
        ],
    )
    .unwrap();
}

/// Insert a block_commits row for the given sortition_id label.
fn insert_block_commit(conn: &Connection, sortition_id_label: &str) {
    conn.execute(
        "INSERT INTO block_commits (txid, vtxindex, block_height, burn_header_hash, \
             sortition_id, block_header_hash, new_seed, parent_block_ptr, parent_vtxindex, \
             key_block_ptr, key_vtxindex, memo, commit_outs, burn_fee, sunset_burn, \
             input, apparent_sender, burn_parent_modulus, punished) \
             VALUES (?1, 0, 1, 'bhh', ?2, 'bhh', 'seed', 0, 0, 0, 0, '', '', '0', '0', \
             'input', 'sender', 0, NULL)",
        params![
            format!("bc_tx_{sortition_id_label}"),
            hex_id(sortition_id_label),
        ],
    )
    .unwrap();
}

/// Insert a block_commit_parents row.
fn insert_block_commit_parent(conn: &Connection, sortition_id_label: &str) {
    conn.execute(
        "INSERT INTO block_commit_parents (block_commit_txid, block_commit_sortition_id, \
             parent_sortition_id) VALUES (?1, ?2, 'parent_sort')",
        params![
            format!("bc_tx_{sortition_id_label}"),
            hex_id(sortition_id_label),
        ],
    )
    .unwrap();
}

/// Insert a stack_stx row for the given burn_header_hash.
fn insert_stack_stx(conn: &Connection, burn_header_hash: &str, txid: &str) {
    conn.execute(
        "INSERT INTO stack_stx (txid, vtxindex, block_height, burn_header_hash, \
             sender_addr, reward_addr, stacked_ustx, num_cycles, signer_key, max_amount, auth_id) \
             VALUES (?1, 0, 1, ?2, 'sender', 'reward', '1000', 1, NULL, NULL, NULL)",
        params![txid, burn_header_hash],
    )
    .unwrap();
}

/// Insert an epochs row.
fn insert_epoch(conn: &Connection, start: u32, epoch_id: u32) {
    conn.execute(
        "INSERT INTO epochs (start_block_height, end_block_height, epoch_id, \
             block_limit, network_epoch) VALUES (?1, ?2, ?3, '{}', 1)",
        params![start, start + 100, epoch_id],
    )
    .unwrap();
}

/// Create a sortition dest DB simulating a squashed MARF with the given
/// canonical sortition-ID labels.
///
/// Each label is stored as raw UTF-8 bytes in the `marf_squashed_blocks` BLOB
/// column, so `lower(hex(block_hash))` returns the hex form that test
/// chainstate inserts use (sortition IDs in `snapshots` are TEXT).
fn create_sortition_dest_db(path: &std::path::Path, canonical_sortition_ids: &[&str]) {
    // A real (tiny) MARF so the leaf walk in `collect_canonical_leaf_hashes`
    // succeeds; its single leaf is irrelevant to the sortition assertions
    // (src `__fork_storage` is empty, so nothing matches it anyway).
    let mut marf = MARF::<SortitionId>::from_path(path.to_str().unwrap(), MARFOpenOpts::default())
        .expect("MARF init failed");
    marf.begin(&SortitionId::sentinel(), &SortitionId([0x99; 32]))
        .unwrap();
    marf.insert("test::leaf", MARFValue([0xff; 40])).unwrap();
    marf.commit().unwrap();
    drop(marf);

    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS marf_squashed_blocks (
            height INTEGER PRIMARY KEY,
            block_hash BLOB NOT NULL UNIQUE,
            marf_root_hash BLOB NOT NULL
        )",
    )
    .unwrap();
    for (h, sid) in canonical_sortition_ids.iter().enumerate() {
        // Store the 32-byte zero-padded id so `lower(hex(block_hash))`
        // matches the `hex_id`-encoded sortition_id in `src.snapshots`,
        // and a 32-byte root hash so the `bulk_read_squashed_blocks`
        // accessor (which validates lengths) accepts it.
        conn.execute(
            "INSERT INTO marf_squashed_blocks (height, block_hash, marf_root_hash) \
             VALUES (?1, ?2, ?3)",
            params![
                h as i64,
                label_block_id(sid).0.as_slice(),
                [0u8; 32].as_slice()
            ],
        )
        .unwrap();
    }
}

fn sortition_test_tip_boundary(max_stacks_height: u64) -> SortitionTipCopyBoundary {
    SortitionTipCopyBoundary {
        max_stacks_height,
        anchor_consensus_hash: ConsensusHash([0x11; 20]),
        anchor_burn_view_consensus_hash: ConsensusHash([0x11; 20]),
        anchor_block_hash: BlockHeaderHash([0x22; 32]),
        anchor_block_height: max_stacks_height,
    }
}

/// Canonical and fork rows across every sortition_id-filtered table:
/// only the canonical sortitions' rows are copied, and validation passes.
#[test]
fn test_sortition_copy_excludes_fork_data() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    // Canonical chain: sort_0 at height 0, sort_1 at height 1.
    insert_snapshot(&conn, "sort_0", "bhh_0", 0);
    insert_snapshot(&conn, "sort_1", "bhh_1", 1);
    // Fork at height 1: sort_1_fork with different burn hash.
    insert_snapshot(&conn, "sort_1_fork", "bhh_1_fork", 1);

    // Insert related data for canonical and fork.
    insert_leader_key(&conn, "sort_1");
    insert_leader_key(&conn, "sort_1_fork");
    insert_block_commit(&conn, "sort_1");
    insert_block_commit(&conn, "sort_1_fork");
    insert_block_commit_parent(&conn, "sort_1");
    insert_block_commit_parent(&conn, "sort_1_fork");
    insert_stack_stx(&conn, "bhh_1", "stx_tx_canon");
    insert_stack_stx(&conn, "bhh_1_fork", "stx_tx_fork");
    insert_epoch(&conn, 0, 1);

    // Transition ops.
    conn.execute(
        "INSERT INTO snapshot_transition_ops (sortition_id, accepted_ops, consumed_keys) \
             VALUES (?1, '[]', '[]')",
        params![hex_id("sort_1")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO snapshot_transition_ops (sortition_id, accepted_ops, consumed_keys) \
             VALUES (?1, '[]', '[]')",
        params![hex_id("sort_1_fork")],
    )
    .unwrap();

    // Stacks chain tips.
    conn.execute(
        "INSERT INTO stacks_chain_tips (sortition_id, consensus_hash, block_hash, block_height) \
             VALUES (?1, 'ch', 'bh', 1)",
        params![hex_id("sort_1")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO stacks_chain_tips (sortition_id, consensus_hash, block_hash, block_height) \
             VALUES (?1, 'ch2', 'bh2', 1)",
        params![hex_id("sort_1_fork")],
    )
    .unwrap();

    // Missed commits.
    conn.execute(
        "INSERT INTO missed_commits (txid, input, intended_sortition_id) \
             VALUES ('mc_tx', 'input', ?1)",
        params![hex_id("sort_1")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO missed_commits (txid, input, intended_sortition_id) \
             VALUES ('mc_tx_fork', 'input', ?1)",
        params![hex_id("sort_1_fork")],
    )
    .unwrap();

    // Preprocessed reward sets.
    conn.execute(
        "INSERT INTO preprocessed_reward_sets (sortition_id, reward_set) \
             VALUES (?1, '{}')",
        params![hex_id("sort_1")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO preprocessed_reward_sets (sortition_id, reward_set) \
             VALUES (?1, '{}')",
        params![hex_id("sort_1_fork")],
    )
    .unwrap();

    drop(conn);

    // Only sort_0 and sort_1 are canonical.
    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0", "sort_1"]);

    let stats =
        copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap()).unwrap();

    // Only canonical rows should be copied.
    assert_eq!(stats.snapshots_rows, 2, "2 canonical snapshots");
    assert_eq!(stats.leader_keys_rows, 1, "only sort_1 leader key");
    assert_eq!(stats.block_commits_rows, 1, "only sort_1 block commit");
    assert_eq!(
        stats.block_commit_parents_rows, 1,
        "only sort_1 block commit parent"
    );
    assert_eq!(
        stats.snapshot_transition_ops_rows, 1,
        "only sort_1 transition ops"
    );
    assert_eq!(stats.stacks_chain_tips_rows, 1, "only sort_1 chain tip");
    assert_eq!(stats.missed_commits_rows, 1, "only sort_1 missed commit");
    assert_eq!(
        stats.preprocessed_reward_sets_rows, 1,
        "only sort_1 reward set"
    );
    assert_eq!(stats.stack_stx_rows, 1, "only bhh_1 stack_stx");
    assert_eq!(stats.epochs_rows, 1, "epochs full copy");
    assert_eq!(stats.db_config_rows, 1, "db_config full copy");

    // Validate passes.
    let validation =
        validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
            .unwrap();
    assert!(
        validation.is_valid(),
        "validation should pass: {validation:?}"
    );
}

/// A corrupted non-key column in a copied `snapshots` row must fail
/// validation (full-row compare, not count-only).
#[test]
fn test_sortition_validate_detects_payload_corruption() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    insert_snapshot(&conn, "sort_0", "bhh_0", 0);
    insert_epoch(&conn, 0, 1);
    drop(conn);

    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0"]);

    let _stats =
        copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap()).unwrap();

    // Corrupt a non-key column in the destination snapshots table.
    {
        let conn = Connection::open(&dst_path).unwrap();
        conn.execute(
            "UPDATE snapshots SET burn_header_timestamp = 9999 WHERE sortition_id = ?1",
            params![hex_id("sort_0")],
        )
        .unwrap();
    }

    let validation =
        validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
            .unwrap();

    assert!(
        !validation.snapshots_match,
        "payload corruption should be detected"
    );
    assert!(!validation.is_valid(), "validation must fail");
}

/// A `leader_keys` row injected into the destination with no source
/// counterpart must fail validation.
#[test]
fn test_sortition_validate_detects_extra_rows() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    insert_snapshot(&conn, "sort_0", "bhh_0", 0);
    insert_epoch(&conn, 0, 1);
    drop(conn);

    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0"]);

    let _stats =
        copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap()).unwrap();

    // Inject an extra leader_keys row in destination that doesn't exist in source.
    {
        let conn = Connection::open(&dst_path).unwrap();
        conn.execute(
            "INSERT INTO leader_keys (txid, vtxindex, block_height, burn_header_hash, \
                 sortition_id, consensus_hash, public_key, memo) \
                 VALUES ('extra_tx', 0, 1, 'bhh', ?1, 'ch', 'pk', 'memo')",
            params![hex_id("sort_0")],
        )
        .unwrap();
    }

    let validation =
        validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
            .unwrap();

    assert!(
        !validation.leader_keys_match,
        "extra rows should be detected"
    );
    assert!(!validation.is_valid(), "validation must fail");
}

/// burn_header_hash-keyed tables (`stack_stx`, `transfer_stx`, ...) must
/// exclude rows associated with non-canonical burn hashes.
#[test]
fn test_sortition_burn_header_hash_filtering() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    insert_snapshot(&conn, "sort_0", "bhh_canon", 0);
    insert_snapshot(&conn, "sort_0_fork", "bhh_fork", 0);

    // stack_stx at canonical and fork burn hashes.
    insert_stack_stx(&conn, "bhh_canon", "stx_canon");
    insert_stack_stx(&conn, "bhh_fork", "stx_fork");

    // transfer_stx at canonical and fork.
    conn.execute(
        "INSERT INTO transfer_stx (txid, vtxindex, block_height, burn_header_hash, \
             sender_addr, recipient_addr, transfered_ustx, memo) \
             VALUES ('xfer_canon', 0, 0, 'bhh_canon', 's', 'r', '100', 'x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO transfer_stx (txid, vtxindex, block_height, burn_header_hash, \
             sender_addr, recipient_addr, transfered_ustx, memo) \
             VALUES ('xfer_fork', 0, 0, 'bhh_fork', 's', 'r', '100', 'x')",
        [],
    )
    .unwrap();

    insert_epoch(&conn, 0, 1);
    drop(conn);

    // Only sort_0 is canonical -> bhh_canon is the only canonical burn hash.
    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0"]);

    let stats =
        copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap()).unwrap();

    assert_eq!(stats.stack_stx_rows, 1, "only bhh_canon stack_stx");
    assert_eq!(stats.transfer_stx_rows, 1, "only bhh_canon transfer_stx");

    let validation =
        validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
            .unwrap();
    assert!(
        validation.is_valid(),
        "should pass with canonical-only data: {validation:?}"
    );
}

/// A destination claiming a canonical sortition_id absent from
/// `src.snapshots` is corruption: the copy must abort.
#[test]
fn test_sortition_copy_rejects_fabricated_canonical_set() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    insert_snapshot(&conn, "sort_0", "bhh_0", 0);
    insert_epoch(&conn, 0, 1);
    drop(conn);

    // Destination claims sort_0 AND sort_fake as canonical.
    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0", "sort_fake"]);

    let err = copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
        .expect_err("copy must reject fabricated canonical sortition");
    match err {
        Error::CorruptionError(msg) => assert!(
            msg.contains("canonical sortition") && msg.contains("absent from src.snapshots"),
            "unexpected corruption message: {msg}"
        ),
        other => panic!("expected CorruptionError, got {other:?}"),
    }
}

/// `stacks_chain_tips_by_burn_view` (schema 11) rows for canonical
/// sortitions are copied and reported in the stats.
#[test]
fn test_sortition_stacks_chain_tips_by_burn_view_copied() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    // Insert canonical snapshots.
    insert_snapshot(&conn, "sort_0", "bhh_0", 0);
    insert_snapshot(&conn, "sort_1", "bhh_1", 1);
    insert_epoch(&conn, 0, 2);

    // Insert stacks_chain_tips_by_burn_view rows (schema 11 table).
    // consensus_hash and burn_view_consensus_hash must reference
    // existing snapshots(consensus_hash) due to FK constraints.
    conn.execute(
        "INSERT INTO stacks_chain_tips_by_burn_view \
         (sortition_id, consensus_hash, burn_view_consensus_hash, block_hash, block_height) \
         VALUES (?1, 'ch_sort_0', 'ch_sort_0', 'bh_0', 0)",
        params![hex_id("sort_0")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO stacks_chain_tips_by_burn_view \
         (sortition_id, consensus_hash, burn_view_consensus_hash, block_hash, block_height) \
         VALUES (?1, 'ch_sort_1', 'ch_sort_1', 'bh_1', 1)",
        params![hex_id("sort_1")],
    )
    .unwrap();
    drop(conn);

    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0", "sort_1"]);

    let stats =
        copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap()).unwrap();

    // The stats struct should reflect the copied rows.
    assert_eq!(
        stats.stacks_chain_tips_by_burn_view_rows, 2,
        "stats should report 2 stacks_chain_tips_by_burn_view rows"
    );

    // Verify the rows actually exist in the destination.
    let dst_conn = Connection::open(&dst_path).unwrap();
    let count: i64 = dst_conn
        .query_row(
            "SELECT COUNT(*) FROM stacks_chain_tips_by_burn_view",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);

    // Validation should pass.
    let validation =
        validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
            .unwrap();
    assert!(
        validation.is_valid(),
        "validation should pass: {validation:?}"
    );
}

/// Sortition tip memo rows pointing above the Stacks boundary are
/// rewritten down to the anchor in both memo tables, and boundary
/// validation accepts the result.
#[test]
fn test_sortition_tip_copy_rewrites_rows_above_stacks_boundary() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    insert_snapshot(&conn, "sort_0", "bhh_0", 0);
    insert_snapshot(&conn, "sort_1", "bhh_1", 1);
    insert_epoch(&conn, 0, 2);

    let boundary = sortition_test_tip_boundary(10);
    let anchor_ch = boundary.anchor_consensus_hash.to_string();
    let anchor_burn_view_ch = boundary.anchor_burn_view_consensus_hash.to_string();
    let anchor_bhh = boundary.anchor_block_hash.to_string();
    let source_tip_bhh = BlockHeaderHash([0x33; 32]).to_string();

    conn.execute(
        "UPDATE snapshots SET consensus_hash = ?1 WHERE sortition_id = ?2",
        params![&anchor_burn_view_ch, hex_id("sort_1")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO stacks_chain_tips \
         (sortition_id, consensus_hash, block_hash, block_height) \
         VALUES (?1, ?2, ?3, 20)",
        params![hex_id("sort_1"), &anchor_ch, &source_tip_bhh],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO stacks_chain_tips_by_burn_view \
         (sortition_id, consensus_hash, burn_view_consensus_hash, block_hash, block_height) \
         VALUES (?1, ?2, ?3, ?4, 20)",
        params![
            hex_id("sort_1"),
            &anchor_ch,
            &anchor_burn_view_ch,
            &source_tip_bhh
        ],
    )
    .unwrap();
    drop(conn);

    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0", "sort_1"]);

    copy_sortition_side_tables_with_boundary(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        Some(&boundary),
    )
    .unwrap();

    let dst_conn = Connection::open(&dst_path).unwrap();
    let old_tip: (String, String, i64) = dst_conn
        .query_row(
            "SELECT consensus_hash, block_hash, block_height FROM stacks_chain_tips \
             WHERE sortition_id = ?1",
            params![hex_id("sort_1")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(old_tip, (anchor_ch.clone(), anchor_bhh.clone(), 10));

    let burn_view_tip: (String, String, String, i64) = dst_conn
        .query_row(
            "SELECT consensus_hash, burn_view_consensus_hash, block_hash, block_height \
             FROM stacks_chain_tips_by_burn_view WHERE sortition_id = ?1",
            params![hex_id("sort_1")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        burn_view_tip,
        (anchor_ch, anchor_burn_view_ch, anchor_bhh, 10)
    );

    let validation = validate_sortition_side_tables_with_boundary(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        Some(&boundary),
    )
    .unwrap();
    assert!(
        validation.stacks_chain_tips_within_stacks_boundary,
        "rewritten tips must be within the Stacks boundary"
    );
    assert!(
        validation.is_valid(),
        "validation should accept rewritten sortition tips: {validation:?}"
    );
}

/// A copy without the boundary rewrite leaves tips above the Stacks
/// boundary; boundary validation must reject them.
#[test]
fn test_sortition_validation_rejects_tip_above_stacks_boundary() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_sort.sqlite");
    let conn = create_sortition_source_db(&src_path);

    insert_snapshot(&conn, "sort_0", "bhh_0", 0);
    insert_snapshot(&conn, "sort_1", "bhh_1", 1);
    insert_epoch(&conn, 0, 2);

    let boundary = sortition_test_tip_boundary(10);
    let anchor_ch = boundary.anchor_consensus_hash.to_string();
    let anchor_burn_view_ch = boundary.anchor_burn_view_consensus_hash.to_string();
    let source_tip_bhh = BlockHeaderHash([0x33; 32]).to_string();

    conn.execute(
        "UPDATE snapshots SET consensus_hash = ?1 WHERE sortition_id = ?2",
        params![&anchor_burn_view_ch, hex_id("sort_1")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO stacks_chain_tips_by_burn_view \
         (sortition_id, consensus_hash, burn_view_consensus_hash, block_hash, block_height) \
         VALUES (?1, ?2, ?3, ?4, 20)",
        params![
            hex_id("sort_1"),
            &anchor_ch,
            &anchor_burn_view_ch,
            &source_tip_bhh
        ],
    )
    .unwrap();
    drop(conn);

    let dst_path = dir.path().join("dst_sort.sqlite");
    create_sortition_dest_db(&dst_path, &["sort_0", "sort_1"]);

    copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap()).unwrap();

    let validation = validate_sortition_side_tables_with_boundary(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        Some(&boundary),
    )
    .unwrap();
    assert!(
        !validation.stacks_chain_tips_within_stacks_boundary,
        "validation must reject copied sortition tips beyond the Stacks boundary"
    );
    assert!(!validation.is_valid(), "validation must fail");
}

/// Drift guard: every table the sortition migrations create must be
/// classified, so a future migration can't silently drop one from the copy.
#[test]
fn test_no_unclassified_sortition_tables() {
    let dir = tempdir().unwrap();
    let conn = create_sortition_source_db(&dir.path().join("src.sqlite"));
    let known: Vec<&str> = super::super::sortition::REQUIRED_TABLES
        .iter()
        .chain(MARF_INFRA_TABLES.iter())
        .copied()
        .collect();
    let extra = unclassified_tables(&conn, &known);
    assert!(
        extra.is_empty(),
        "unclassified sortition table(s) {extra:?}: classify each in REQUIRED_TABLES \
         (snapshot/sortition.rs)"
    );
}
