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

//! SPV headers DB (headers.sqlite) copy/validate tests.

use rstest::rstest;
use rusqlite::{params, Connection};
use tempfile::tempdir;

use super::super::common::{unclassified_tables, MARF_INFRA_TABLES};
use crate::burnchains::bitcoin::spv::{
    SPV_DB_VERSION, SPV_INITIAL_SCHEMA, SPV_SCHEMA_2, SPV_SCHEMA_3,
};

/// Drift guard: every table the SPV migrations create must be
/// classified, so a future migration can't silently drop one from the copy.
#[test]
fn test_no_unclassified_spv_tables() {
    let dir = tempdir().unwrap();
    let conn = create_spv_headers_db(&dir.path().join("src.sqlite"));
    let known: Vec<&str> = super::super::spv::REQUIRED_TABLES
        .iter()
        .chain(MARF_INFRA_TABLES.iter())
        .copied()
        .collect();
    let extra = unclassified_tables(&conn, &known);
    assert!(
        extra.is_empty(),
        "unclassified SPV table(s) {extra:?}: classify each in REQUIRED_TABLES (snapshot/spv.rs)"
    );
}

/// Create a source headers.sqlite (SPV v3 schema with chain_work).
/// Replays the real SPV migration pipeline: INITIAL -> SCHEMA_2 -> SCHEMA_3.
fn create_spv_headers_db(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    for cmd in SPV_INITIAL_SCHEMA {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SPV_SCHEMA_2 {
        conn.execute_batch(cmd).unwrap();
    }
    for cmd in SPV_SCHEMA_3 {
        conn.execute_batch(cmd).unwrap();
    }
    conn.execute(
        &format!("INSERT INTO db_config (version) VALUES ('{SPV_DB_VERSION}')"),
        [],
    )
    .unwrap();
    conn
}

/// Headers are copied up to the burn height and `chain_work` only for
/// complete 2016-block intervals; validation passes.
#[test]
fn test_spv_headers_copy_and_validate() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src_headers.sqlite");
    let dst_path = dir.path().join("dst_headers.sqlite");

    let src = create_spv_headers_db(&src_path);
    // Insert headers at heights 0..=5000.
    for h in 0..=5000u32 {
        src.execute(
            "INSERT INTO headers VALUES (1, 'prev', 'merkle', 0, 0, 0, ?1, ?2)",
            params![h, format!("hash_{h}")],
        )
        .unwrap();
    }
    // Insert chain_work for intervals 0, 1, 2.
    src.execute("INSERT INTO chain_work VALUES (0, 'work_0')", [])
        .unwrap();
    src.execute("INSERT INTO chain_work VALUES (1, 'work_1')", [])
        .unwrap();
    src.execute("INSERT INTO chain_work VALUES (2, 'work_2')", [])
        .unwrap();
    drop(src);

    let stats = super::super::spv::copy_spv_headers(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        4500,
    )
    .unwrap();

    // Headers 0..=4500 = 4501 rows.
    assert_eq!(stats.headers_rows, 4501);
    // Interval 0: (0+1)*2016-1=2015 <= 4500 ✓
    // Interval 1: (1+1)*2016-1=4031 <= 4500 ✓
    // Interval 2: (2+1)*2016-1=6047 <= 4500 ✗
    assert_eq!(stats.chain_work_rows, 2);

    let v = super::super::spv::validate_spv_headers(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        4500,
    )
    .unwrap();
    assert!(v.is_valid(), "validation failed: {v:?}");
}

/// Chain-work interval boundary cases: interval `k` is copied iff it is
/// complete at the squash height, i.e. `(k + 1) * 2016 - 1 <= burn_height`.
/// Headers are always copied through `burn_height` inclusive; the source
/// holds one more `chain_work` interval than the expected copy where
/// possible, so each case proves the cutoff.
#[rstest]
#[case::below_first_interval_end(0, 1, 0)]
#[case::exactly_first_interval_end(2015, 2, 1)]
#[case::one_past_first_interval_end(2016, 2, 1)]
#[case::exactly_second_interval_end(4031, 3, 2)]
#[case::one_past_second_interval_end(4032, 3, 2)]
fn test_spv_headers_chain_work_boundaries(
    #[case] burn_height: u32,
    #[case] src_chain_work_intervals: u32,
    #[case] expected_chain_work_rows: u64,
) {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("dst.sqlite");

    let src = create_spv_headers_db(&src_path);
    for h in 0..=burn_height {
        src.execute(
            "INSERT INTO headers VALUES (1, 'p', 'm', 0, 0, 0, ?1, ?2)",
            params![h, format!("h{h}")],
        )
        .unwrap();
    }
    for interval in 0..src_chain_work_intervals {
        src.execute(
            "INSERT INTO chain_work VALUES (?1, ?2)",
            params![interval, format!("w{interval}")],
        )
        .unwrap();
    }
    drop(src);

    let stats = super::super::spv::copy_spv_headers(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        burn_height,
    )
    .unwrap();

    assert_eq!(stats.headers_rows, u64::from(burn_height) + 1);
    assert_eq!(stats.chain_work_rows, expected_chain_work_rows);
}

/// A missing source headers.sqlite is an error.
#[test]
fn test_spv_headers_missing_source_is_error() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("nonexistent.sqlite");
    let dst_path = dir.path().join("dst.sqlite");

    let result = super::super::spv::copy_spv_headers(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        100,
    );
    assert!(result.is_err(), "missing source should error");
}

/// Validating a missing destination is an error.
#[test]
fn test_spv_headers_validate_source_present_dest_missing_fails() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let dst_path = dir.path().join("nonexistent.sqlite");

    create_spv_headers_db(&src_path);

    let result = super::super::spv::validate_spv_headers(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        100,
    );
    assert!(result.is_err());
}

/// Validating with both source and destination missing is an error.
#[test]
fn test_spv_headers_validate_both_absent_is_error() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("no_src.sqlite");
    let dst_path = dir.path().join("no_dst.sqlite");

    let result = super::super::spv::validate_spv_headers(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        100,
    );
    assert!(result.is_err(), "both absent should error");
}

/// A stale destination file must not mask a missing source: the copy
/// still errors.
#[test]
fn test_spv_headers_stale_destination_errors_when_source_absent() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("nonexistent.sqlite");
    let dst_path = dir.path().join("stale_headers.sqlite");

    // Create a stale destination file (simulates reused output dir).
    std::fs::write(&dst_path, b"stale data").unwrap();
    assert!(dst_path.exists());

    let result = super::super::spv::copy_spv_headers(
        src_path.to_str().unwrap(),
        dst_path.to_str().unwrap(),
        100,
    );
    assert!(
        result.is_err(),
        "missing source should error even with stale destination"
    );
}
