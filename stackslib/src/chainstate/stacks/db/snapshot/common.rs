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

use std::io::{Cursor, Read as _, Seek, SeekFrom};
use std::time::Instant;

use rusqlite::{params, Connection};
use stacks_common::util::hash::to_hex;

use crate::chainstate::stacks::index::node::TrieNodeType;
use crate::chainstate::stacks::index::squash::deserialize_node;
use crate::chainstate::stacks::index::Error;

/// A spec for copying a single table from the ATTACHed `src` database.
///
/// The `source_sql` is the exact `SELECT` used to filter source rows.
/// Copy uses plain `INSERT ... SELECT` (no `OR IGNORE`) so that unexpected
/// pre-population in the destination fails loudly.
pub struct TableCopySpec {
    pub table: &'static str,
    /// The exact SELECT for the source side, e.g.
    /// `"SELECT * FROM src.snapshots WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)"`.
    pub source_sql: String,
}

/// Clone table and index schemas from the source DB (via `sqlite_master`) into the
/// destination connection. This avoids duplicating any CREATE TABLE / ALTER TABLE /
/// CREATE INDEX statements and is always in sync with whatever migration version the
/// source is at.
///
/// Expects the source DB to be ATTACHed as `src`.
pub fn clone_schemas_from_source(conn: &Connection, tables: &[&str]) -> Result<(), Error> {
    let mut stmts: Vec<String> = Vec::new();

    for table in tables {
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM src.sqlite_master WHERE type='table' AND name=?1",
                params![table],
                |row| row.get(0),
            )
            .ok();

        if let Some(create_sql) = sql {
            let safe_sql = create_sql.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1);
            stmts.push(safe_sql);
        }

        let mut idx_stmt = conn
            .prepare("SELECT sql FROM src.sqlite_master WHERE type='index' AND tbl_name=?1 AND sql IS NOT NULL")
            .map_err(Error::SQLError)?;
        let idx_rows = idx_stmt
            .query_map(params![table], |row| row.get::<_, String>(0))
            .map_err(Error::SQLError)?;
        for idx_sql in idx_rows {
            let idx_sql = idx_sql.map_err(Error::SQLError)?;
            let safe_sql = idx_sql.replacen("CREATE INDEX", "CREATE INDEX IF NOT EXISTS", 1);
            stmts.push(safe_sql);
        }
    }

    for stmt in &stmts {
        conn.execute_batch(stmt).map_err(Error::SQLError)?;
    }

    Ok(())
}

/// Clone schemas only for tables that exist in the source DB.
/// Returns the list of tables that were actually cloned.
pub fn clone_optional_schemas_from_source(
    conn: &Connection,
    tables: &[&str],
) -> Result<Vec<String>, Error> {
    let mut present = Vec::new();
    for table in tables {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM src.sqlite_master WHERE type='table' AND name=?1",
                params![table],
                |row| row.get(0),
            )
            .map_err(Error::SQLError)?;
        if exists {
            clone_schemas_from_source(conn, &[table])?;
            present.push(table.to_string());
        }
    }
    Ok(present)
}

/// Check if a table exists in the given schema prefix (empty for main, "src" for attached).
pub fn table_exists(conn: &Connection, schema: &str, table: &str) -> bool {
    let master = if schema.is_empty() {
        "sqlite_master".to_string()
    } else {
        format!("{schema}.sqlite_master")
    };
    conn.query_row(
        &format!("SELECT COUNT(*) > 0 FROM {master} WHERE type='table' AND name=?1"),
        params![table],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

/// Check bidirectional full-row EXCEPT equality.
/// Returns true if the two result sets are identical.
pub fn full_row_except_match(conn: &Connection, dst_sql: &str, src_sql: &str) -> bool {
    let extra_in_dst: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM ({dst_sql} EXCEPT {src_sql})"),
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    let extra_in_src: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM ({src_sql} EXCEPT {dst_sql})"),
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    extra_in_dst == 0 && extra_in_src == 0
}

/// Execute a slice of copy specs inside the current transaction.
/// Returns a vec of (table_name, rows_copied).
pub fn execute_copy_specs(
    conn: &Connection,
    specs: &[TableCopySpec],
) -> Result<Vec<(&'static str, u64)>, Error> {
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        let t = Instant::now();
        let sql = format!("INSERT INTO {} {}", spec.table, spec.source_sql);
        let rows = conn.execute(&sql, []).map_err(Error::SQLError)? as u64;
        info!(
            "  copy: {} ({} rows) in {:?}",
            spec.table,
            rows,
            t.elapsed()
        );
        results.push((spec.table, rows));
    }
    Ok(results)
}

/// Check an optional table's match status.
/// Returns None if absent in both, Some(false) if present in one but not other,
/// Some(true/false) from full-row EXCEPT if present in both.
pub fn check_optional_table_match(
    conn: &Connection,
    table: &str,
    src_filter: Option<&str>,
) -> Option<bool> {
    let in_dst = table_exists(conn, "", table);
    let in_src = table_exists(conn, "src", table);

    match (in_dst, in_src) {
        (false, false) => None,
        (true, false) | (false, true) => Some(false),
        (true, true) => {
            let src_sql = match src_filter {
                Some(filter) => format!("SELECT * FROM src.{table} {filter}"),
                None => format!("SELECT * FROM src.{table}"),
            };
            Some(full_row_except_match(
                conn,
                &format!("SELECT * FROM {table}"),
                &src_sql,
            ))
        }
    }
}

/// Read the squashed trie blob from `marf_data` and return its raw bytes.
///
/// Handles both inline blobs (sortition MARF) and external .blobs files
/// (index/clarity MARFs). `dst_path` is the path to the squashed DB file
/// (used to derive the `.blobs` companion path).
fn read_squash_blob(conn: &Connection, dst_path: &str) -> Result<Vec<u8>, Error> {
    // The squash block is the only non-sentinel entry in marf_data.
    let row: (Option<Vec<u8>>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT data, external_offset, external_length \
             FROM marf_data WHERE block_hash != ?1 LIMIT 1",
            params!["sentinel"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(Error::SQLError)?;

    match row {
        (_, Some(offset), Some(length)) if length > 0 => {
            let blobs_path = format!("{dst_path}.blobs");
            let mut file = std::fs::File::open(&blobs_path).map_err(Error::IOError)?;
            file.seek(SeekFrom::Start(offset as u64))
                .map_err(Error::IOError)?;
            let mut buf = vec![0u8; length as usize];
            file.read_exact(&mut buf).map_err(Error::IOError)?;
            Ok(buf)
        }
        (Some(data), _, _) if !data.is_empty() => Ok(data),
        _ => Err(Error::CorruptionError(
            "No squash blob found in marf_data".to_string(),
        )),
    }
}

/// Copy only the `__fork_storage` rows that are referenced by leaf nodes
/// in the squashed MARF trie. Non-canonical entries from forks are excluded.
///
/// The squashed trie blob contains only canonical leaf nodes; each leaf
/// carries a 40-byte `MARFValue` whose hex encoding matches the
/// `value_hash` column in `__fork_storage`.
///
/// Falls back to a full copy if `marf_data` is absent (e.g. in test
/// fixtures that don't go through `squash_to_path`).
///
/// Returns the number of rows copied.
pub fn copy_canonical_fork_storage(conn: &Connection, dst_path: &str) -> Result<u64, Error> {
    // Check if the source even has __fork_storage (test fixtures may not).
    let src_has_table: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM src.sqlite_master WHERE type='table' AND name='__fork_storage'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !src_has_table {
        info!("  copy_canonical_fork_storage: source has no __fork_storage, skipping");
        return Ok(0);
    }

    // Ensure the destination table exists (clone schema from source).
    clone_schemas_from_source(conn, &["__fork_storage"])?;

    // If marf_data doesn't exist, fall back to full copy.
    let has_marf_data: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='marf_data'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !has_marf_data {
        let rows = conn
            .execute(
                "INSERT OR REPLACE INTO __fork_storage SELECT * FROM src.__fork_storage",
                [],
            )
            .map_err(Error::SQLError)? as u64;
        info!("  copy_canonical_fork_storage: no marf_data table, full copy ({rows} rows)");
        return Ok(rows);
    }

    let t = Instant::now();
    let blob = read_squash_blob(conn, dst_path)?;

    // Build a temp table of canonical leaf value hashes.
    conn.execute_batch("CREATE TEMP TABLE __squash_leaf_values (value_hash TEXT PRIMARY KEY)")
        .map_err(Error::SQLError)?;

    let mut cursor = Cursor::new(&blob);
    let blob_len = blob.len() as u64;
    let mut insert_count: u64 = 0;

    {
        let mut stmt = conn
            .prepare("INSERT OR IGNORE INTO __squash_leaf_values (value_hash) VALUES (?1)")
            .map_err(Error::SQLError)?;

        while cursor.position() < blob_len {
            let node = deserialize_node(&mut cursor)?;
            if let TrieNodeType::Leaf(ref leaf) = node {
                stmt.execute(params![to_hex(&leaf.data.to_vec())])
                    .map_err(Error::SQLError)?;
                insert_count += 1;
            }
        }
    }

    info!(
        "  copy_canonical_fork_storage: extracted {insert_count} leaf hashes in {:?}",
        t.elapsed()
    );

    // Copy only the referenced rows.
    let t2 = Instant::now();
    let rows = conn
        .execute(
            "INSERT OR REPLACE INTO __fork_storage \
             SELECT f.* FROM src.__fork_storage f \
             INNER JOIN __squash_leaf_values lv ON f.value_hash = lv.value_hash",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    conn.execute_batch("DROP TABLE IF EXISTS __squash_leaf_values")
        .map_err(Error::SQLError)?;

    info!(
        "  copy_canonical_fork_storage: copied {rows} rows (from {insert_count} leaves) in {:?}",
        t2.elapsed()
    );

    Ok(rows)
}
