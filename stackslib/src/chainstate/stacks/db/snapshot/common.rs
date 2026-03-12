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

use std::time::Instant;

use rusqlite::{params, Connection};

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

/// How a table should be validated after copy.
pub enum ValidationMode {
    /// Bidirectional - full row equality.
    ExactRowsEq,
    /// Count equality only (cheaper for large tables).
    CountEq,
    /// No extra rows in destination beyond what source has.
    NoExtraRows,
    /// Table must be empty in destination.
    MustBeEmpty,
}

/// A spec for validating a single table after copy.
pub struct TableValidationSpec {
    pub table: &'static str,
    /// Filtered SELECT for the source side.
    pub src_sql: String,
    /// SELECT for the destination side (often just `"SELECT * FROM {table}"`).
    pub dst_sql: String,
    pub mode: ValidationMode,
}

/// Result of validating a single table.
pub struct TableValidationResult {
    pub table: &'static str,
    pub passed: bool,
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

/// Check count equality between two SQL queries.
pub fn count_match(conn: &Connection, src_sql: &str, dst_sql: &str) -> bool {
    let src_count: i64 = conn.query_row(src_sql, [], |row| row.get(0)).unwrap_or(-1);
    let dst_count: i64 = conn.query_row(dst_sql, [], |row| row.get(0)).unwrap_or(-2);
    src_count == dst_count
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

/// Execute a slice of validation specs.
/// Returns a vec of per-table results.
pub fn execute_validation_specs(
    conn: &Connection,
    specs: &[TableValidationSpec],
) -> Vec<TableValidationResult> {
    specs
        .iter()
        .map(|spec| {
            let passed = match spec.mode {
                ValidationMode::ExactRowsEq => {
                    full_row_except_match(conn, &spec.dst_sql, &spec.src_sql)
                }
                ValidationMode::CountEq => count_match(conn, &spec.src_sql, &spec.dst_sql),
                ValidationMode::NoExtraRows => {
                    // Check no rows in dst that aren't in src
                    let extra: i64 = conn
                        .query_row(
                            &format!(
                                "SELECT COUNT(*) FROM ({} EXCEPT {})",
                                spec.dst_sql, spec.src_sql
                            ),
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or(1);
                    extra == 0
                }
                ValidationMode::MustBeEmpty => {
                    let count: i64 = conn
                        .query_row(
                            &format!("SELECT COUNT(*) FROM ({})", spec.dst_sql),
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or(1);
                    count == 0
                }
            };
            TableValidationResult {
                table: spec.table,
                passed,
            }
        })
        .collect()
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
