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

use std::collections::HashSet;
use std::time::Instant;

use clarity::vm::database::clarity_store::make_contract_hash_key;
use clarity::vm::database::SqliteConnection;
use clarity::vm::types::QualifiedContractIdentifier;
use rusqlite::Connection;
use stacks_common::types::chainstate::StacksBlockId;

use super::common::{with_indexes_dropped, with_offline_write_session};
use super::fork_storage::{collect_leaf_value_hashes, copy_leaf_referenced_rows};
use crate::chainstate::stacks::index::marf::{MARFOpenOpts, MarfConnection as _, MARF};
use crate::chainstate::stacks::index::storage::TrieHashCalculationMode;
use crate::chainstate::stacks::index::{trie_sql, Error};

/// Copy Clarity side-storage tables (`data_table`, `metadata_table`) from a
/// source MARF database to a squashed MARF database.
///
/// **Must be called after [`MARF::squash_to_path`]** has created the squashed
/// trie in `dst_db_path`.
///
/// This function:
/// 1. Initialises the Clarity schema on the destination (tables + indices + WAL).
/// 2. Attaches the source database.
/// 3. Reads the squashed trie to determine which side-storage rows are still reachable.
/// 4. Copies only the required rows in a single transaction.
pub fn copy_clarity_side_tables(
    src_db_path: &str,
    dst_db_path: &str,
) -> Result<ClaritySideTableStats, Error> {
    let total_start = Instant::now();

    // Walk the squashed trie before opening dst for writes. we need
    // the readonly MARF view, and `marf_sqlite_open` would fight the
    // writer's lock on dst.
    let t = Instant::now();
    let (squashed_tip, needed_keys) = collect_leaf_value_hashes::<StacksBlockId>(dst_db_path)?;
    info!(
        "[clarity] collect_leaf_value_hashes: {} keys in {:?}",
        needed_keys.len(),
        t.elapsed()
    );

    let required_contract_ids = resolve_required_contracts(src_db_path, &squashed_tip)?;

    // `initialize_conn` issues `PRAGMA journal_mode`, which SQLite
    // forbids inside any transaction. Run it on a separate conn
    // before the helper opens its own and enters `BEGIN IMMEDIATE`.
    {
        let init_conn = Connection::open(dst_db_path).map_err(Error::SQLError)?;
        SqliteConnection::initialize_conn(&init_conn).map_err(|e| {
            Error::CorruptionError(format!("Failed to initialize Clarity schema: {e:?}"))
        })?;
    }

    let stats = with_offline_write_session(
        dst_db_path,
        &[("src", src_db_path)],
        "",
        |conn| -> Result<ClaritySideTableStats, Error> {
            let t = Instant::now();
            let src_data_count: u64 = conn
                .query_row("SELECT COUNT(*) FROM src.data_table", [], |row| row.get(0))
                .map_err(Error::SQLError)?;
            let needed_count = needed_keys.len() as u64;
            let pruned_count = src_data_count.saturating_sub(needed_count);
            info!(
                "[clarity] src.data_table = {src_data_count}, pruning {pruned_count} \
                 (keep {needed_count}) in {:?}",
                t.elapsed()
            );

            // data_table is content-addressed (key = hex MARFValue), like
            // the index `__fork_storage`, so it shares the same stream-filter.
            let data_rows = copy_leaf_referenced_rows(conn, "data_table", "key", &needed_keys)?;

            let t = Instant::now();
            let (metadata_scanned, metadata_rows) =
                with_indexes_dropped(conn, "metadata_table", |conn| {
                    copy_required_metadata_rows(conn, &required_contract_ids)
                })?;
            info!(
                "[clarity] metadata_table scan+filter: scanned {metadata_scanned}, \
                 inserted {metadata_rows} in {:?}",
                t.elapsed()
            );

            Ok(ClaritySideTableStats {
                data_table_rows: data_rows,
                metadata_table_rows: metadata_rows,
            })
        },
    )?;

    info!("[clarity] total {:?}", total_start.elapsed());
    Ok(stats)
}

/// Stream `src.metadata_table` into the destination `metadata_table`,
/// keeping only rows whose contract id is in `required`. Rows whose key is
/// not in the [`SqliteConnection`] metadata format are skipped.
/// Returns `(scanned, copied)` row counts.
fn copy_required_metadata_rows(
    conn: &Connection,
    required: &HashSet<String>,
) -> Result<(u64, u64), Error> {
    let mut stmt = conn
        .prepare("SELECT key, blockhash, value FROM src.metadata_table")
        .map_err(Error::SQLError)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(Error::SQLError)?;
    let mut insert = conn
        .prepare(
            "INSERT INTO metadata_table (key, blockhash, value) \
             VALUES (?1, ?2, ?3)",
        )
        .map_err(Error::SQLError)?;
    let mut scanned: u64 = 0;
    let mut copied: u64 = 0;
    for row in rows {
        scanned += 1;
        let (key, blockhash, value) = row.map_err(Error::SQLError)?;
        let Some((contract_id, _meta_key)) = SqliteConnection::parse_metadata_key(&key) else {
            continue;
        };
        if !required.contains(contract_id) {
            continue;
        }
        insert
            .execute([key, blockhash, value])
            .map_err(Error::SQLError)?;
        copied += 1;
    }
    Ok((scanned, copied))
}

/// The distinct contract ids appearing in `metadata_table` keys on `conn`.
/// Scanned in key order so the result is deterministic across runs.
fn scan_metadata_contract_ids(conn: &Connection) -> Result<Vec<String>, Error> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();
    let mut stmt = conn
        .prepare("SELECT key FROM metadata_table ORDER BY key")
        .map_err(Error::SQLError)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(Error::SQLError)?;
    for row in rows {
        let key = row.map_err(Error::SQLError)?;
        if let Some((contract_id, _meta_key)) = SqliteConnection::parse_metadata_key(&key) {
            if seen.insert(contract_id.to_string()) {
                ordered.push(contract_id.to_string());
            }
        }
    }
    Ok(ordered)
}

/// Probe the MARF at `db_path` for each contract's hash key at `tip`; the
/// contracts still present in the trie are the ones whose metadata rows
/// must be retained.
fn filter_required_contracts(
    db_path: &str,
    tip: &StacksBlockId,
    contract_ids: &[String],
) -> Result<HashSet<String>, Error> {
    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, "noop", true);
    let mut marf = MARF::<StacksBlockId>::from_path(db_path, open_opts)?;
    let mut required: HashSet<String> = HashSet::new();
    for contract_id in contract_ids {
        let contract = QualifiedContractIdentifier::parse(contract_id).map_err(|e| {
            Error::CorruptionError(format!(
                "Failed to parse contract identifier '{contract_id}': {e:?}"
            ))
        })?;
        let key = make_contract_hash_key(&contract);
        if marf.get(tip, &key)?.is_some() {
            required.insert(contract_id.clone());
        }
    }
    Ok(required)
}

/// Scan `src.metadata_table` for the set of contract ids that appear,
/// then probe the squashed trie to find which are still required.
fn resolve_required_contracts(
    src_db_path: &str,
    squashed_tip: &StacksBlockId,
) -> Result<HashSet<String>, Error> {
    let t = Instant::now();
    let src_conn = Connection::open_with_flags(
        src_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(Error::SQLError)?;
    let contract_ids = scan_metadata_contract_ids(&src_conn)?;
    info!(
        "[clarity] contract ids in src.metadata_table: {} unique in {:?}",
        contract_ids.len(),
        t.elapsed()
    );

    let t = Instant::now();
    let required_contract_ids =
        filter_required_contracts(src_db_path, squashed_tip, &contract_ids)?;
    info!(
        "[clarity] MARF.get per contract: {} required of {} in {:?}",
        required_contract_ids.len(),
        contract_ids.len(),
        t.elapsed()
    );

    Ok(required_contract_ids)
}

/// Row-count statistics returned by [`copy_clarity_side_tables`].
#[derive(Debug, Clone)]
pub struct ClaritySideTableStats {
    /// Number of rows copied into `data_table`.
    pub data_table_rows: u64,
    /// Number of rows copied into `metadata_table`.
    pub metadata_table_rows: u64,
}

/// Validate that a squashed Clarity MARF's side tables match the source.
///
/// Checks:
/// - All trie-referenced `data_table` keys are present in the destination.
/// - All required `metadata_table` rows (exhaustive across all contracts) are present.
/// - A diagnostic sample of contracts is reported for troubleshooting.
pub fn validate_clarity_side_tables(
    src_db_path: &str,
    dst_db_path: &str,
) -> Result<ClaritySideTableValidation, Error> {
    let src_conn = Connection::open_with_flags(
        src_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(Error::SQLError)?;

    let dst_conn = Connection::open_with_flags(
        dst_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(Error::SQLError)?;

    let src_data_rows: u64 =
        src_conn.query_row("SELECT COUNT(*) FROM data_table", [], |row| row.get(0))?;
    let dst_data_rows: u64 =
        dst_conn.query_row("SELECT COUNT(*) FROM data_table", [], |row| row.get(0))?;

    let src_meta_rows: u64 =
        src_conn.query_row("SELECT COUNT(*) FROM metadata_table", [], |row| row.get(0))?;
    let dst_meta_rows: u64 =
        dst_conn.query_row("SELECT COUNT(*) FROM metadata_table", [], |row| row.get(0))?;

    const SAMPLE_CONTRACT_LIMIT: usize = 20;
    let all_contract_ids_ordered = scan_metadata_contract_ids(&src_conn)?;
    let dst_tip = trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(&dst_conn)?;

    let sample_contract_ids: Vec<&str> = all_contract_ids_ordered
        .iter()
        .take(SAMPLE_CONTRACT_LIMIT)
        .map(|s| s.as_str())
        .collect();

    let mut sample_contracts_checked: u64 = 0;
    let mut sample_contracts_missing_in_trie: u64 = 0;
    let mut sample_contracts_missing_in_data_table: u64 = 0;

    if !sample_contract_ids.is_empty() {
        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, "noop", true);
        let mut marf = MARF::<StacksBlockId>::from_path(dst_db_path, open_opts)?;

        for contract_id in sample_contract_ids.iter() {
            sample_contracts_checked += 1;
            let contract = QualifiedContractIdentifier::parse(contract_id).map_err(|e| {
                Error::CorruptionError(format!(
                    "Failed to parse contract identifier '{contract_id}': {e:?}"
                ))
            })?;
            let key = make_contract_hash_key(&contract);
            let trie_value = marf.get(&dst_tip, &key)?;
            let Some(trie_value) = trie_value else {
                sample_contracts_missing_in_trie += 1;
                continue;
            };

            let side_key = trie_value.to_hex();
            let exists: bool = dst_conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM data_table WHERE key = ?1",
                    [side_key],
                    |row| row.get(0),
                )
                .map_err(Error::SQLError)?;
            if !exists {
                sample_contracts_missing_in_data_table += 1;
            }
        }
    }

    let (_tip, needed_keys) = collect_leaf_value_hashes::<StacksBlockId>(dst_db_path)?;
    dst_conn
        .execute("ATTACH DATABASE ?1 AS src", [src_db_path])
        .map_err(Error::SQLError)?;
    dst_conn
        .execute_batch("CREATE TEMP TABLE trie_values (key TEXT PRIMARY KEY)")
        .map_err(Error::SQLError)?;
    {
        let mut stmt = dst_conn
            .prepare("INSERT INTO trie_values (key) VALUES (?1)")
            .map_err(Error::SQLError)?;
        for key in needed_keys.iter() {
            stmt.execute([key.to_hex()]).map_err(Error::SQLError)?;
        }
    }
    let missing_required_data_table_keys: u64 = dst_conn
        .query_row(
            "SELECT COUNT(*) FROM src.data_table \
         WHERE key IN (SELECT key FROM trie_values) \
           AND key NOT IN (SELECT key FROM data_table)",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    dst_conn
        .execute_batch("DETACH src")
        .map_err(Error::SQLError)?;

    let required_contract_ids =
        filter_required_contracts(dst_db_path, &dst_tip, &all_contract_ids_ordered)?;

    let mut missing_required_metadata_rows: u64 = 0;
    {
        let mut stmt = src_conn
            .prepare("SELECT key, blockhash, value FROM metadata_table")
            .map_err(Error::SQLError)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(Error::SQLError)?;
        for row in rows {
            let (key, blockhash, value) = row.map_err(Error::SQLError)?;
            let Some((contract_id, _meta_key)) = SqliteConnection::parse_metadata_key(&key) else {
                continue;
            };
            if !required_contract_ids.contains(contract_id) {
                continue;
            }
            let exists: bool = dst_conn.query_row(
                "SELECT COUNT(*) > 0 FROM metadata_table WHERE key = ?1 AND blockhash = ?2 AND value = ?3",
                [key, blockhash, value],
                |row| row.get(0),
            )?;
            if !exists {
                missing_required_metadata_rows += 1;
            }
        }
    }

    Ok(ClaritySideTableValidation {
        required_data_keys_present: missing_required_data_table_keys == 0,
        src_data_table_rows: src_data_rows,
        dst_data_table_rows: dst_data_rows,
        required_metadata_present: missing_required_metadata_rows == 0,
        src_metadata_table_rows: src_meta_rows,
        dst_metadata_table_rows: dst_meta_rows,
        sample_contracts_checked,
        sample_contracts_missing_in_trie,
        sample_contracts_missing_in_data_table,
        missing_required_data_table_keys,
        missing_required_metadata_rows,
    })
}

/// Validation results for Clarity side tables.
#[derive(Debug, Clone)]
pub struct ClaritySideTableValidation {
    /// All trie-referenced data_table keys are present in the destination.
    pub required_data_keys_present: bool,
    /// Source `data_table` row count.
    pub src_data_table_rows: u64,
    /// Destination `data_table` row count.
    pub dst_data_table_rows: u64,
    /// All required metadata rows (for contracts with trie commitments) are
    /// present in the destination. Checked exhaustively over all contracts.
    pub required_metadata_present: bool,
    /// Source `metadata_table` row count.
    pub src_metadata_table_rows: u64,
    /// Destination `metadata_table` row count.
    pub dst_metadata_table_rows: u64,
    /// Number of contract identifiers sampled from metadata_table (diagnostic).
    pub sample_contracts_checked: u64,
    /// Sampled contracts missing from the trie (diagnostic, should be 0).
    pub sample_contracts_missing_in_trie: u64,
    /// Sampled contracts whose trie values are missing from data_table (diagnostic, should be 0).
    pub sample_contracts_missing_in_data_table: u64,
    /// Required data_table keys missing from destination (should be 0).
    pub missing_required_data_table_keys: u64,
    /// Required metadata rows missing from destination (should be 0).
    pub missing_required_metadata_rows: u64,
}

impl ClaritySideTableValidation {
    /// Returns `true` if all required data and metadata are present.
    pub fn is_valid(&self) -> bool {
        self.required_data_keys_present && self.required_metadata_present
    }
}
