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

use std::fs;
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};

use super::common::{
    clone_schemas_from_source, copied_rows, execute_copy_specs, full_row_except_match,
    with_offline_write_session, TableCopySpec,
};
use crate::chainstate::stacks::index::Error;

/// Tables required in all burnchain.sqlite versions.
pub(super) const REQUIRED_TABLES: &[&str] = &[
    "burnchain_db_block_headers",
    "burnchain_db_block_ops",
    "block_commit_metadata",
    "anchor_blocks",
    "overrides",
    "db_config",
];

/// Row-count statistics returned by [`copy_burnchain_db`].
#[derive(Debug, Clone)]
pub struct BurnchainDbCopyStats {
    pub block_headers_rows: u64,
    pub block_ops_rows: u64,
    pub block_commit_metadata_rows: u64,
    pub anchor_blocks_rows: u64,
    pub overrides_rows: u64,
}

/// Validation result for a copied burnchain.sqlite.
#[derive(Debug, Clone)]
pub struct BurnchainDbValidation {
    pub block_headers_match: bool,
    pub block_ops_match: bool,
    pub block_commit_metadata_match: bool,
    pub anchor_blocks_match: bool,
    pub overrides_match: bool,
    pub db_config_match: bool,
    pub no_extra_headers: bool,
    pub canonical_complete: bool,
}

impl BurnchainDbValidation {
    pub fn is_valid(&self) -> bool {
        self.block_headers_match
            && self.block_ops_match
            && self.block_commit_metadata_match
            && self.anchor_blocks_match
            && self.overrides_match
            && self.db_config_match
            && self.no_extra_headers
            && self.canonical_complete
    }
}

/// Build a temp table of canonical burn header hashes from the squashed
/// sortition DB (ATTACHed as `sort`).
fn populate_canonical_burn_hashes(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "CREATE TEMP TABLE canonical_burn_hashes (burn_header_hash TEXT PRIMARY KEY)",
    )
    .map_err(Error::SQLError)?;
    conn.execute(
        "INSERT INTO canonical_burn_hashes (burn_header_hash) \
         SELECT DISTINCT burn_header_hash FROM sort.snapshots",
        [],
    )
    .map_err(Error::SQLError)?;
    Ok(())
}

/// Copy canonical rows from source `burnchain.sqlite` into a new destination,
/// using the squashed sortition DB as the authoritative canonical set.
///
/// Preserves dependency-ordered copy:
/// 1. Canonical headers and ops (burn_header_hash filtered)
/// 2. Canonical block_commit_metadata
/// 3. anchor_blocks derived from copied commit metadata
/// 4. overrides derived from copied anchor blocks
pub fn copy_burnchain_db(
    src_burnchain_db_path: &str,
    dst_burnchain_db_path: &str,
    squashed_sortition_path: &str,
    expected_burn_height: u32,
) -> Result<BurnchainDbCopyStats, Error> {
    if !Path::new(src_burnchain_db_path).exists() {
        return Err(Error::CorruptionError(format!(
            "Source burnchain DB not found: {src_burnchain_db_path}"
        )));
    }
    if !Path::new(squashed_sortition_path).exists() {
        return Err(Error::CorruptionError(format!(
            "Squashed sortition DB not found: {squashed_sortition_path}"
        )));
    }

    if let Some(parent) = Path::new(dst_burnchain_db_path).parent() {
        fs::create_dir_all(parent).map_err(Error::IOError)?;
    }

    with_offline_write_session(
        dst_burnchain_db_path,
        &[
            ("src", src_burnchain_db_path),
            ("sort", squashed_sortition_path),
        ],
        // FK off must run before BEGIN IMMEDIATE; per-connection only.
        "PRAGMA foreign_keys = OFF;",
        |conn| copy_burnchain_db_inner(conn, expected_burn_height),
    )
}

/// Build the copy specs for the burnchain DB, in dependency order:
/// canonical headers and ops (burn-hash filtered), commit metadata,
/// `anchor_blocks` derived from the copied commit metadata, and `overrides`
/// derived from the copied anchor blocks.
fn burnchain_copy_specs() -> Vec<TableCopySpec> {
    let bhh = "SELECT burn_header_hash FROM canonical_burn_hashes";
    vec![
        TableCopySpec {
            table: "db_config",
            source_sql: "SELECT * FROM src.db_config".into(),
        },
        TableCopySpec {
            table: "burnchain_db_block_headers",
            source_sql: format!(
                "SELECT * FROM src.burnchain_db_block_headers WHERE block_hash IN ({bhh})"
            ),
        },
        TableCopySpec {
            table: "burnchain_db_block_ops",
            source_sql: format!(
                "SELECT * FROM src.burnchain_db_block_ops WHERE block_hash IN ({bhh})"
            ),
        },
        TableCopySpec {
            table: "block_commit_metadata",
            source_sql: format!(
                "SELECT * FROM src.block_commit_metadata WHERE burn_block_hash IN ({bhh})"
            ),
        },
        TableCopySpec {
            table: "anchor_blocks",
            source_sql: "SELECT * FROM src.anchor_blocks \
                 WHERE reward_cycle IN ( \
                     SELECT DISTINCT anchor_block FROM block_commit_metadata \
                     WHERE anchor_block IS NOT NULL \
                 )"
            .into(),
        },
        TableCopySpec {
            table: "overrides",
            source_sql: "SELECT * FROM src.overrides \
                 WHERE reward_cycle IN (SELECT reward_cycle FROM anchor_blocks)"
                .into(),
        },
    ]
}

/// Consistency assertion: the squashed sortition DB's tip (ATTACHed as
/// `sort`) must match the caller's expected burn height.
fn assert_sortition_tip_height(conn: &Connection, expected_burn_height: u32) -> Result<(), Error> {
    let actual_max_height: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(block_height), 0) FROM sort.snapshots",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    if actual_max_height != expected_burn_height {
        return Err(Error::CorruptionError(format!(
            "Sortition tip height mismatch: expected {expected_burn_height}, got {actual_max_height}"
        )));
    }
    Ok(())
}

fn copy_burnchain_db_inner(
    conn: &Connection,
    expected_burn_height: u32,
) -> Result<BurnchainDbCopyStats, Error> {
    clone_schemas_from_source(conn, REQUIRED_TABLES)?;

    // Build canonical burn hash set from squashed sortition DB.
    populate_canonical_burn_hashes(conn)?;

    assert_sortition_tip_height(conn, expected_burn_height)?;

    // Completeness assertion: every canonical burn hash must exist in source.
    let missing_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM canonical_burn_hashes \
             WHERE burn_header_hash NOT IN \
                 (SELECT block_hash FROM src.burnchain_db_block_headers)",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    if missing_count > 0 {
        return Err(Error::CorruptionError(format!(
            "{missing_count} canonical burn hashes missing from source burnchain DB"
        )));
    }

    let results = execute_copy_specs(conn, &burnchain_copy_specs())?;

    Ok(BurnchainDbCopyStats {
        block_headers_rows: copied_rows(&results, "burnchain_db_block_headers"),
        block_ops_rows: copied_rows(&results, "burnchain_db_block_ops"),
        block_commit_metadata_rows: copied_rows(&results, "block_commit_metadata"),
        anchor_blocks_rows: copied_rows(&results, "anchor_blocks"),
        overrides_rows: copied_rows(&results, "overrides"),
    })
}

/// Validate a copied burnchain.sqlite against its source.
pub fn validate_burnchain_db(
    src_burnchain_db_path: &str,
    dst_burnchain_db_path: &str,
    squashed_sortition_path: &str,
    expected_burn_height: u32,
) -> Result<BurnchainDbValidation, Error> {
    let conn = Connection::open_with_flags(dst_burnchain_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(Error::SQLError)?;

    conn.execute("ATTACH DATABASE ?1 AS src", params![src_burnchain_db_path])
        .map_err(Error::SQLError)?;
    conn.execute(
        "ATTACH DATABASE ?1 AS sort",
        params![squashed_sortition_path],
    )
    .map_err(Error::SQLError)?;

    populate_canonical_burn_hashes(&conn)?;

    assert_sortition_tip_height(&conn, expected_burn_height)?;

    // Completeness: every canonical burn hash must be present in destination.
    let missing_in_dst: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM canonical_burn_hashes \
             WHERE burn_header_hash NOT IN \
                 (SELECT block_hash FROM burnchain_db_block_headers)",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    let canonical_complete = missing_in_dst == 0;

    let bhh = "SELECT burn_header_hash FROM canonical_burn_hashes";

    let db_config_match = full_row_except_match(
        &conn,
        "SELECT * FROM db_config",
        "SELECT * FROM src.db_config",
    )?;

    let block_headers_match = full_row_except_match(
        &conn,
        "SELECT * FROM burnchain_db_block_headers",
        &format!("SELECT * FROM src.burnchain_db_block_headers WHERE block_hash IN ({bhh})"),
    )?;

    let block_ops_match = full_row_except_match(
        &conn,
        "SELECT * FROM burnchain_db_block_ops",
        &format!("SELECT * FROM src.burnchain_db_block_ops WHERE block_hash IN ({bhh})"),
    )?;

    let block_commit_metadata_match = full_row_except_match(
        &conn,
        "SELECT * FROM block_commit_metadata",
        &format!("SELECT * FROM src.block_commit_metadata WHERE burn_block_hash IN ({bhh})"),
    )?;

    let anchor_blocks_match = full_row_except_match(
        &conn,
        "SELECT * FROM anchor_blocks",
        "SELECT * FROM src.anchor_blocks \
         WHERE reward_cycle IN ( \
             SELECT DISTINCT anchor_block FROM block_commit_metadata \
             WHERE anchor_block IS NOT NULL \
         )",
    )?;

    let overrides_match = full_row_except_match(
        &conn,
        "SELECT * FROM overrides",
        "SELECT * FROM src.overrides \
         WHERE reward_cycle IN (SELECT reward_cycle FROM anchor_blocks)",
    )?;

    // No non-canonical burn hashes in destination.
    let extra_non_canonical: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM burnchain_db_block_headers \
                 WHERE block_hash NOT IN ({bhh})"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    let no_extra_headers = extra_non_canonical == 0;

    conn.execute_batch("DETACH DATABASE sort")
        .map_err(Error::SQLError)?;
    conn.execute_batch("DETACH DATABASE src")
        .map_err(Error::SQLError)?;

    Ok(BurnchainDbValidation {
        block_headers_match,
        block_ops_match,
        block_commit_metadata_match,
        anchor_blocks_match,
        overrides_match,
        db_config_match,
        no_extra_headers,
        canonical_complete,
    })
}
