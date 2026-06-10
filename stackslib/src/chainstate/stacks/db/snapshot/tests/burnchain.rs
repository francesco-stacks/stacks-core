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

//! Burnchain DB (burnchain.sqlite) copy/validate tests.

use rusqlite::{params, Connection};
use tempfile::tempdir;

use super::super::common::{unclassified_tables, MARF_INFRA_TABLES};
use crate::burnchains::db::{
    BURNCHAIN_DB_INDEXES, BURNCHAIN_DB_MIGRATION_V2_TO_V3, BURNCHAIN_DB_SCHEMA_2,
};

/// Drift guard: every table the burnchain migrations create must be
/// classified, so a future migration can't silently drop one from the copy.
#[test]
fn test_no_unclassified_burnchain_tables() {
    let dir = tempdir().unwrap();
    let conn = create_burnchain_db(&dir.path().join("src.sqlite"));
    let known: Vec<&str> = super::super::burnchain::REQUIRED_TABLES
        .iter()
        .chain(MARF_INFRA_TABLES.iter())
        .copied()
        .collect();
    let extra = unclassified_tables(&conn, &known);
    assert!(
        extra.is_empty(),
        "unclassified burnchain table(s) {extra:?}: classify each in REQUIRED_TABLES \
         (snapshot/burnchain.rs)"
    );
}

/// Create a burnchain.sqlite source.
/// Replays the real schema: SCHEMA_2 then MIGRATION_V2_TO_V3, plus indexes.
fn create_burnchain_db(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(BURNCHAIN_DB_SCHEMA_2).unwrap();
    conn.execute("INSERT INTO db_config (version) VALUES ('2')", [])
        .unwrap();
    for idx in BURNCHAIN_DB_INDEXES {
        conn.execute_batch(idx).unwrap();
    }
    conn.execute_batch(BURNCHAIN_DB_MIGRATION_V2_TO_V3).unwrap();
    conn.execute("UPDATE db_config SET version = '3'", [])
        .unwrap();
    conn
}

/// Create a squashed sortition DB with canonical burn hashes in a
/// `snapshots` table.
fn create_squashed_sortition(
    path: &std::path::Path,
    canonical_hashes: &[(u32, &str)], // (block_height, burn_header_hash)
) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE snapshots (
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL
            )",
    )
    .unwrap();
    for (height, hash) in canonical_hashes {
        conn.execute(
            "INSERT INTO snapshots (block_height, burn_header_hash) VALUES (?1, ?2)",
            params![height, hash],
        )
        .unwrap();
    }
    conn
}

/// End-to-end burnchain copy: canonical headers, ops, and commit
/// metadata are copied, fork rows are dropped, and validation passes.
#[test]
fn test_burnchain_db_copy_and_validate() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_burnchain.sqlite");
    let dst_path = dir.path().join("dst_burnchain.sqlite");
    let sort_path = dir.path().join("sortition.sqlite");

    // Canonical hashes at heights 0, 1, 2.
    let canonical = vec![(0, "hash_0"), (1, "hash_1"), (2, "hash_2")];
    create_squashed_sortition(&sort_path, &canonical);

    let src = create_burnchain_db(&src_path);
    // Insert canonical block headers.
    for (h, hash) in &canonical {
        src.execute(
            "INSERT INTO burnchain_db_block_headers VALUES (?1, ?2, ?3, 0, 0)",
            params![h, hash, format!("parent_{hash}")],
        )
        .unwrap();
    }
    // Insert a non-canonical block at height 1.
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (1, 'fork_hash_1', 'parent_fork', 0, 0)",
        [],
    )
    .unwrap();
    // Ops for canonical and non-canonical.
    src.execute(
        "INSERT INTO burnchain_db_block_ops VALUES ('hash_1', 'op1', 'tx1')",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_ops VALUES ('fork_hash_1', 'op_fork', 'tx_fork')",
        [],
    )
    .unwrap();
    // block_commit_metadata for canonical.
    src.execute(
            "INSERT INTO block_commit_metadata (burn_block_hash, txid, block_height, vtxindex, anchor_block, anchor_block_descendant) \
             VALUES ('hash_1', 'tx1', 1, 0, NULL, NULL)",
            [],
        )
        .unwrap();
    // block_commit_metadata for non-canonical.
    src.execute(
            "INSERT INTO block_commit_metadata (burn_block_hash, txid, block_height, vtxindex, anchor_block, anchor_block_descendant) \
             VALUES ('fork_hash_1', 'tx_fork', 1, 0, NULL, NULL)",
            [],
        )
        .unwrap();
    drop(src);

    let stats = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        2,
    )
    .unwrap();

    assert_eq!(stats.block_headers_rows, 3); // 3 canonical
    assert_eq!(stats.block_ops_rows, 1); // only hash_1's op
    assert_eq!(stats.block_commit_metadata_rows, 1); // only canonical

    let v = super::super::burnchain::validate_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        2,
    )
    .unwrap();
    assert!(v.is_valid(), "validation failed: {v:?}");
}

/// Two headers at the same height: only the one whose hash is in the
/// squashed sortition snapshots is copied.
#[test]
fn test_burnchain_db_excludes_non_canonical_fork() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    // Only hash_a is canonical at height 1.
    create_squashed_sortition(&sort_path, &[(0, "genesis"), (1, "hash_a")]);

    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'genesis', 'none', 0, 0)",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (1, 'hash_a', 'genesis', 0, 0)",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (1, 'hash_b', 'genesis', 0, 0)",
        [],
    )
    .unwrap();
    drop(src);

    let stats = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        1,
    )
    .unwrap();

    assert_eq!(stats.block_headers_rows, 2); // genesis + hash_a, not hash_b

    // Verify hash_b is not in destination.
    let dst = Connection::open(&dst_path).unwrap();
    let count: i64 = dst
        .query_row(
            "SELECT COUNT(*) FROM burnchain_db_block_headers WHERE block_hash = 'hash_b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

/// Block ops follow their header's canonicality: ops of a fork header
/// are dropped.
#[test]
fn test_burnchain_db_block_ops_follow_canonical_headers() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    create_squashed_sortition(&sort_path, &[(0, "canon")]);

    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'canon', 'none', 0, 0)",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'fork', 'none', 0, 0)",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_ops VALUES ('canon', 'op_c', 'tx_c')",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_ops VALUES ('fork', 'op_f', 'tx_f')",
        [],
    )
    .unwrap();
    drop(src);

    let stats = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        0,
    )
    .unwrap();

    assert_eq!(stats.block_ops_rows, 1);

    let dst = Connection::open(&dst_path).unwrap();
    let op: String = dst
        .query_row("SELECT op FROM burnchain_db_block_ops", [], |r| r.get(0))
        .unwrap();
    assert_eq!(op, "op_c");
}

/// `anchor_blocks` and `overrides` are restricted to reward cycles
/// referenced by the copied commit metadata.
#[test]
fn test_burnchain_db_anchor_blocks_filtered() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    create_squashed_sortition(&sort_path, &[(0, "h0"), (1, "h1")]);

    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'h0', 'none', 0, 0)",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (1, 'h1', 'h0', 0, 0)",
        [],
    )
    .unwrap();
    // Anchor block for cycle 1 (referenced by canonical commit).
    src.execute("INSERT INTO anchor_blocks VALUES (1)", [])
        .unwrap();
    // Anchor block for cycle 99 (not referenced by any canonical commit).
    src.execute("INSERT INTO anchor_blocks VALUES (99)", [])
        .unwrap();
    // Canonical commit referencing anchor block cycle 1.
    src.execute(
            "INSERT INTO block_commit_metadata (burn_block_hash, txid, block_height, vtxindex, anchor_block, anchor_block_descendant) \
             VALUES ('h1', 'tx_a', 1, 0, 1, NULL)",
            [],
        )
        .unwrap();
    // Override for cycle 1 (should be copied) and cycle 99 (should not).
    src.execute("INSERT INTO overrides VALUES (1, 'map_1')", [])
        .unwrap();
    src.execute("INSERT INTO overrides VALUES (99, 'map_99')", [])
        .unwrap();
    drop(src);

    let stats = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        1,
    )
    .unwrap();

    assert_eq!(stats.anchor_blocks_rows, 1);
    assert_eq!(stats.overrides_rows, 1);

    let dst = Connection::open(&dst_path).unwrap();
    let cycle: i64 = dst
        .query_row("SELECT reward_cycle FROM anchor_blocks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cycle, 1);
    let override_map: String = dst
        .query_row("SELECT affirmation_map FROM overrides", [], |r| r.get(0))
        .unwrap();
    assert_eq!(override_map, "map_1");
}

/// A non-canonical header injected into the destination must fail
/// validation (`no_extra_headers`).
#[test]
fn test_burnchain_db_validate_detects_non_canonical_leak() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    create_squashed_sortition(&sort_path, &[(0, "h0")]);

    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'h0', 'none', 0, 0)",
        [],
    )
    .unwrap();
    drop(src);

    // Copy normally first.
    super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        0,
    )
    .unwrap();

    // Inject a non-canonical row into the destination.
    let dst = Connection::open(&dst_path).unwrap();
    dst.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'rogue', 'none', 0, 0)",
        [],
    )
    .unwrap();
    drop(dst);

    let v = super::super::burnchain::validate_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        0,
    )
    .unwrap();
    assert!(!v.is_valid(), "should detect non-canonical leak");
    assert!(!v.no_extra_headers);
}

/// A missing source burnchain.sqlite is an error.
#[test]
fn test_burnchain_db_missing_source_is_error() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("nonexistent.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    create_squashed_sortition(&sort_path, &[(0, "h0")]);

    let result = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        0,
    );
    // Should error because the source file does not exist.
    assert!(result.is_err());
}

/// A sortition tip height that disagrees with the caller's expected burn
/// height is corruption: the copy must abort.
#[test]
fn test_burnchain_db_sortition_tip_mismatch_is_error() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    // Sortition tip is at height 5.
    create_squashed_sortition(&sort_path, &[(0, "h0"), (5, "h5")]);

    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'h0', 'none', 0, 0)",
        [],
    )
    .unwrap();
    drop(src);

    // Pass expected_burn_height=10, but sortition tip is 5.
    let result = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        10,
    );
    assert!(result.is_err(), "should fail on sortition tip mismatch");
}

/// The copy creates missing parent directories for the destination path.
#[test]
fn test_burnchain_db_fresh_output_dir() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let sort_path = dir.path().join("sort.sqlite");
    // Nested non-existent directory.
    let dst_path = dir
        .path()
        .join("deep")
        .join("nested")
        .join("burnchain.sqlite");

    create_squashed_sortition(&sort_path, &[(0, "h0")]);

    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'h0', 'none', 0, 0)",
        [],
    )
    .unwrap();
    drop(src);

    let stats = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        0,
    )
    .unwrap();

    assert_eq!(stats.block_headers_rows, 1);
    assert!(dst_path.exists());
}

/// A canonical burn hash absent from the source headers is corruption:
/// the copy must abort.
#[test]
fn test_burnchain_db_copy_fails_when_source_missing_canonical_hash() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    // Sortition says heights 0, 1, 2 are canonical.
    create_squashed_sortition(&sort_path, &[(0, "h0"), (1, "h1"), (2, "h2")]);

    // But source burnchain.sqlite only has h0 and h1 - h2 is missing.
    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'h0', 'none', 0, 0)",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (1, 'h1', 'h0', 0, 0)",
        [],
    )
    .unwrap();
    drop(src);

    let result = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        2,
    );
    assert!(
        result.is_err(),
        "should fail when source is missing a canonical burn hash"
    );
}

/// A canonical header deleted from the destination must fail validation
/// (`canonical_complete`).
#[test]
fn test_burnchain_db_validate_detects_missing_canonical_hash() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    create_squashed_sortition(&sort_path, &[(0, "h0"), (1, "h1")]);

    let src = create_burnchain_db(&src_path);
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (0, 'h0', 'none', 0, 0)",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO burnchain_db_block_headers VALUES (1, 'h1', 'h0', 0, 0)",
        [],
    )
    .unwrap();
    drop(src);

    // Copy normally (source has all canonical hashes).
    super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        1,
    )
    .unwrap();

    // Now delete h1 from the destination to simulate incomplete copy.
    let dst = Connection::open(&dst_path).unwrap();
    dst.execute(
        "DELETE FROM burnchain_db_block_headers WHERE block_hash = 'h1'",
        [],
    )
    .unwrap();
    drop(dst);

    let v = super::super::burnchain::validate_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        1,
    )
    .unwrap();
    assert!(!v.is_valid(), "should detect missing canonical hash: {v:?}");
    assert!(!v.canonical_complete);
}

/// The read-only ATTACH of a missing source must not create the file as
/// a side effect.
#[test]
fn test_burnchain_db_missing_source_does_not_create_file() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("nonexistent_burnchain.sqlite");
    let dst_path = dir.path().join("dst.sqlite");
    let sort_path = dir.path().join("sort.sqlite");

    create_squashed_sortition(&sort_path, &[(0, "h0")]);

    assert!(!src_path.exists());

    let result = super::super::burnchain::copy_burnchain_db(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        sort_path.to_str().unwrap(),
        0,
    );
    assert!(result.is_err());
    // Source path must not have been created by ATTACH.
    assert!(
        !src_path.exists(),
        "missing source must not be created by ATTACH"
    );
}
