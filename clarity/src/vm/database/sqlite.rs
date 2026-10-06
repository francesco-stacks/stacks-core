// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use clarity_types::errors::IncomparableError;
use rusqlite::{Connection, OptionalExtension, params};
use stacks_common::types::chainstate::{BlockHeaderHash, StacksBlockId, TrieHash};
use stacks_common::types::sqlite::NO_PARAMS;
use stacks_common::util::db::tx_busy_handler;
use stacks_common::util::hash::Sha512Trunc256Sum;

use super::clarity_store::{ContractCommitment, MetadataValue, make_contract_hash_key};
use super::{
    ClarityBackingStore, ClarityDatabase, ClarityDeserializable, NULL_BURN_STATE_DB,
    NULL_HEADER_DB, SpecialCaseHandler,
};
use crate::vm::analysis::{AnalysisDatabase, RuntimeCheckErrorKind};
use crate::vm::errors::{RuntimeError, VmExecutionError, VmInternalError};
use crate::vm::types::QualifiedContractIdentifier;

/// Indexed reads of the persistent metadata table.
const METADATA_BATCH_QUERY: &str = "SELECT requested.key, metadata.value
    FROM json_each(?1) AS requested LEFT JOIN metadata_table AS metadata
    ON metadata.blockhash = ?2 AND metadata.key = requested.value";

const SQL_FAIL_MESSAGE: &str = "PANIC: SQL Failure in Smart Contract VM.";

/// Clarity side-storage table names.
pub const DATA_TABLE_NAME: &str = "data_table";
pub const METADATA_TABLE_NAME: &str = "metadata_table";

/// A borrowed `metadata_table` row, yielded by
/// [`SqliteConnection::visit_metadata_rows`] and accepted by
/// [`SqliteConnection::insert_metadata_row`].
pub struct MetadataRow<'a> {
    /// Full `clr-meta::<contract id>::<key>` key.
    pub key: &'a str,
    /// The `blockhash` column: a hex-encoded [`StacksBlockId`].
    pub block_id: &'a str,
    /// The stored metadata value.
    pub value: rusqlite::types::ValueRef<'a>,
}

/// Stateless storage operations on caller-owned SQLite connections.
pub enum SqliteConnection {}

fn sqlite_put(conn: &Connection, key: &str, value: &str) -> Result<(), VmExecutionError> {
    let params = params![key, value];
    match conn
        .prepare_cached("REPLACE INTO data_table (key, value) VALUES (?, ?)")
        .and_then(|mut stmt| stmt.execute(params))
    {
        Ok(_) => Ok(()),
        Err(e) => {
            error!("Failed to insert/replace ({key},{value}): {e:?}");
            Err(VmInternalError::DBError(SQL_FAIL_MESSAGE.into()).into())
        }
    }
}

fn sqlite_get(conn: &Connection, key: &str) -> Result<Option<String>, VmExecutionError> {
    trace!("sqlite_get {key}");
    let params = params![key];
    let res = match conn
        .prepare_cached("SELECT value FROM data_table WHERE key = ?")
        .and_then(|mut stmt| stmt.query_row(params, |row| row.get(0)))
        .optional()
    {
        Ok(x) => Ok(x),
        Err(e) => {
            error!("Failed to query '{key}': {e:?}");
            Err(VmInternalError::DBError(SQL_FAIL_MESSAGE.into()).into())
        }
    };

    trace!("sqlite_get {key}: {res:?}");
    res
}

fn sqlite_has_entry(conn: &Connection, key: &str) -> Result<bool, VmExecutionError> {
    Ok(sqlite_get(conn, key)?.is_some())
}

pub fn sqlite_get_contract_hash(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
    let key = make_contract_hash_key(contract);
    let contract_commitment = store
        .get_data(&key)?
        .map(|x| ContractCommitment::deserialize(&x))
        .ok_or_else(|| RuntimeCheckErrorKind::NoSuchContract(contract.to_string()))?;
    let ContractCommitment {
        block_height,
        hash: contract_hash,
    } = contract_commitment?;
    let bhh = store.get_block_at_height(block_height)
            .ok_or_else(|| VmInternalError::Expect("Should always be able to map from height to block hash when looking up contract information.".into()))?;
    Ok((bhh, contract_hash))
}

pub fn sqlite_insert_metadata(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
    key: &str,
    value: &str,
) -> Result<(), VmExecutionError> {
    let bhh = store.get_open_chain_tip();
    SqliteConnection::insert_metadata(
        store.get_side_store(),
        &bhh,
        &contract.to_string(),
        key,
        value,
    )
}

pub fn sqlite_get_metadata(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
    key: &str,
) -> Result<Option<String>, VmExecutionError> {
    let (bhh, _) = store.get_contract_hash(contract)?;
    SqliteConnection::get_metadata(store.get_side_store(), &bhh, &contract.to_string(), key)
}

/// Copy a SQLite value into rollback-aware metadata storage.
fn metadata_value(value: rusqlite::types::ValueRef<'_>) -> Result<MetadataValue, VmExecutionError> {
    match value {
        rusqlite::types::ValueRef::Text(bytes) => String::from_utf8(bytes.to_vec())
            .map(MetadataValue::Text)
            .map_err(|e| VmInternalError::DBError(e.to_string()).into()),
        rusqlite::types::ValueRef::Blob(bytes) => Ok(MetadataValue::Blob(bytes.into())),
        _ => Err(VmInternalError::DBError("Invalid metadata storage type".into()).into()),
    }
}

/// Commit an already-encoded executable row at the open deployment tip.
pub fn sqlite_insert_metadata_value(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
    key: &str,
    value: &MetadataValue,
) -> Result<(), VmExecutionError> {
    let block = store.get_open_chain_tip();
    match value {
        MetadataValue::Text(value) => SqliteConnection::insert_metadata(
            store.get_side_store(),
            &block,
            &contract.to_string(),
            key,
            value,
        ),
        MetadataValue::Blob(bytes) => SqliteConnection::insert_metadata_bytes(
            store.get_side_store(),
            &block,
            &contract.to_string(),
            key,
            bytes,
        ),
    }
}

/// Ordinary metadata is text. Executable records are read through the raw batch API.
fn metadata_text(value: rusqlite::types::ValueRef<'_>) -> Result<String, VmExecutionError> {
    match value {
        rusqlite::types::ValueRef::Text(bytes) => String::from_utf8(bytes.to_vec())
            .map_err(|e| VmInternalError::DBError(e.to_string()).into()),
        rusqlite::types::ValueRef::Blob(_) => Err(VmInternalError::DBError(
            "Expected text metadata, found executable BLOB".into(),
        )
        .into()),
        _ => Err(VmInternalError::DBError("Invalid metadata storage type".into()).into()),
    }
}

/// Read raw records in caller order with one deployment resolution.
/// Reuse a deployment only within the caller's unchanged contract/view scope.
pub fn sqlite_get_metadata_batch(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
    keys: &[String],
    deployment: &mut Option<StacksBlockId>,
) -> Result<Vec<Option<MetadataValue>>, VmExecutionError> {
    sqlite_get_metadata_batch_with_query(store, contract, keys, deployment, METADATA_BATCH_QUERY)
}

/// Run a store-specific indexed query. `query` must return (requested array index,
/// raw value), with JSON keys bound as ?1 and deployment block bound as ?2.
/// Ephemeral stores must push these predicates into both UNION branches;
/// SQLite 3.45 does not push a key-IN subquery through their metadata view.
pub fn sqlite_get_metadata_batch_with_query(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
    keys: &[String],
    deployment: &mut Option<StacksBlockId>,
    query: &str,
) -> Result<Vec<Option<MetadataValue>>, VmExecutionError> {
    if keys.is_empty() {
        return Ok(vec![]);
    }
    let bhh = match deployment.as_ref() {
        Some(block) => block.clone(),
        None => {
            let (block, _) = store.get_contract_hash(contract)?;
            *deployment = Some(block.clone());
            block
        }
    };
    let contract_id = contract.to_string();
    let metadata_keys: Vec<_> = keys
        .iter()
        .map(|key| SqliteConnection::make_metadata_key(&contract_id, key))
        .collect();
    let requested = serde_json::to_string(&metadata_keys)
        .map_err(|e| VmInternalError::DBError(e.to_string()))?;
    let conn = store.get_side_store();
    let mut statement = conn
        .prepare_cached(query)
        .map_err(|e| VmInternalError::DBError(e.to_string()))?;
    let mut rows = statement
        .query(params![requested, bhh])
        .map_err(|e| VmInternalError::DBError(e.to_string()))?;
    let mut values = vec![None; keys.len()];
    while let Some(row) = rows
        .next()
        .map_err(|e| VmInternalError::DBError(e.to_string()))?
    {
        let index: usize = row
            .get(0)
            .map_err(|e| VmInternalError::DBError(e.to_string()))?;
        let value = values
            .get_mut(index)
            .ok_or_else(|| VmInternalError::Expect("Invalid metadata batch index".into()))?;
        let raw = row
            .get_ref(1)
            .map_err(|e| VmInternalError::DBError(e.to_string()))?;
        if raw != rusqlite::types::ValueRef::Null {
            *value = Some(metadata_value(raw)?);
        }
    }
    Ok(values)
}

pub fn sqlite_get_metadata_manual(
    store: &mut dyn ClarityBackingStore,
    at_height: u32,
    contract: &QualifiedContractIdentifier,
    key: &str,
) -> Result<Option<String>, VmExecutionError> {
    let bhh = store.get_block_at_height(at_height).ok_or_else(|| {
        warn!("Unknown block height when manually querying metadata"; "block_height" => at_height);
        RuntimeError::BadBlockHeight(at_height.to_string())
    })?;
    SqliteConnection::get_metadata(store.get_side_store(), &bhh, &contract.to_string(), key)
}

impl SqliteConnection {
    /// Build a `metadata_table` key: `clr-meta::<contract id>::<metadata key>`.
    fn make_metadata_key(contract_id: &str, key: &str) -> String {
        format!("clr-meta::{contract_id}::{key}")
    }

    /// Split a `metadata_table` key produced by [`Self::make_metadata_key`]
    /// back into `(contract id, metadata key)`. Returns `None` if `key` is
    /// not in that format. The contract id never contains `::`, so the first
    /// separator after the prefix is the boundary (the metadata key may
    /// itself contain `::`).
    pub fn parse_metadata_key(key: &str) -> Option<(&str, &str)> {
        key.strip_prefix("clr-meta::")?.split_once("::")
    }

    pub fn put(conn: &Connection, key: &str, value: &str) -> Result<(), VmExecutionError> {
        sqlite_put(conn, key, value)
    }

    pub fn get(conn: &Connection, key: &str) -> Result<Option<String>, VmExecutionError> {
        sqlite_get(conn, key)
    }

    pub fn insert_metadata(
        conn: &Connection,
        bhh: &StacksBlockId,
        contract_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), VmExecutionError> {
        let key = Self::make_metadata_key(contract_id, key);
        let params = params![bhh, key, value];

        if let Err(e) = conn
            .prepare_cached("INSERT INTO metadata_table (blockhash, key, value) VALUES (?, ?, ?)")
            .and_then(|mut stmt| stmt.execute(params))
        {
            error!("Failed to insert metadata ({bhh},{key}): {e:?}");
            return Err(VmInternalError::DBError(SQL_FAIL_MESSAGE.into()).into());
        }
        Ok(())
    }

    pub fn insert_metadata_bytes(
        conn: &Connection,
        bhh: &StacksBlockId,
        contract_id: &str,
        key: &str,
        value: &[u8],
    ) -> Result<(), VmExecutionError> {
        let key = Self::make_metadata_key(contract_id, key);
        conn.prepare_cached("INSERT INTO metadata_table (blockhash, key, value) VALUES (?, ?, ?)")
            .and_then(|mut stmt| stmt.execute(params![bhh, key, value]))
            .map_err(|e| VmInternalError::DBError(e.to_string()))?;
        Ok(())
    }

    /// Insert a `metadata_table` row verbatim. [`MetadataRow::key`] is assumed
    /// to already be in the [`Self::make_metadata_key`] (`clr-meta::`) format.
    pub fn insert_metadata_row(
        conn: &Connection,
        row: &MetadataRow,
    ) -> Result<(), rusqlite::Error> {
        conn.prepare_cached("INSERT INTO metadata_table (blockhash, key, value) VALUES (?, ?, ?)")?
            .execute(params![
                row.block_id,
                row.key,
                rusqlite::types::ToSqlOutput::Borrowed(row.value)
            ])?;
        Ok(())
    }

    /// Visit every `metadata_table` row on `conn` as a [`MetadataRow`].
    pub fn visit_metadata_rows<E, F>(conn: &Connection, mut visit: F) -> Result<(), E>
    where
        E: From<rusqlite::Error>,
        F: FnMut(&MetadataRow) -> Result<(), E>,
    {
        let mut stmt = conn.prepare("SELECT key, blockhash, value FROM metadata_table")?;
        let mut rows = stmt.query(NO_PARAMS)?;
        while let Some(row) = rows.next()? {
            let key = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            let block_id = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            let value = row.get_ref(2)?;
            visit(&MetadataRow {
                key,
                block_id,
                value,
            })?;
        }
        Ok(())
    }

    /// Visit every `metadata_table` key on `conn` in ascending key order.
    ///
    /// The ascending order is a guaranteed part of this method's contract
    /// (`ORDER BY key`); callers such as the snapshot copier rely on it for
    /// deterministic, reproducible scans (locked by `metadata_keys_visited_in_order`).
    pub fn visit_metadata_keys<E, F>(conn: &Connection, mut visit: F) -> Result<(), E>
    where
        E: From<rusqlite::Error>,
        F: FnMut(&str) -> Result<(), E>,
    {
        let mut stmt = conn.prepare("SELECT key FROM metadata_table ORDER BY key")?;
        let mut rows = stmt.query(NO_PARAMS)?;
        while let Some(row) = rows.next()? {
            visit(row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?)?;
        }
        Ok(())
    }

    /// Number of rows in `data_table`.
    pub fn count_data_rows(conn: &Connection) -> Result<u64, rusqlite::Error> {
        conn.query_row("SELECT COUNT(*) FROM data_table", NO_PARAMS, |row| {
            row.get(0)
        })
    }

    pub fn commit_metadata_to(
        conn: &Connection,
        from: &StacksBlockId,
        to: &StacksBlockId,
    ) -> Result<(), VmExecutionError> {
        let params = params![to, from];
        if let Err(e) = conn.execute(
            "UPDATE metadata_table SET blockhash = ? WHERE blockhash = ?",
            params,
        ) {
            error!("Failed to update {from} to {to}: {e:?}");
            return Err(VmInternalError::DBError(SQL_FAIL_MESSAGE.into()).into());
        }
        Ok(())
    }

    pub fn drop_metadata(conn: &Connection, from: &StacksBlockId) -> Result<(), VmExecutionError> {
        if let Err(e) = conn.execute(
            "DELETE FROM metadata_table WHERE blockhash = ?",
            params![from],
        ) {
            error!("Failed to drop metadata from {from}: {e:?}");
            return Err(VmInternalError::DBError(SQL_FAIL_MESSAGE.into()).into());
        }
        Ok(())
    }

    pub fn get_metadata(
        conn: &Connection,
        bhh: &StacksBlockId,
        contract_id: &str,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        let key = Self::make_metadata_key(contract_id, key);
        let params = params![bhh, key];

        match conn
            .prepare_cached("SELECT value FROM metadata_table WHERE blockhash = ? AND key = ?")
            .and_then(|mut stmt| stmt.query_row(params, |row| Ok(metadata_text(row.get_ref(0)?))))
            .optional()
        {
            Ok(x) => x.transpose(),
            Err(e) => {
                error!("Failed to query ({bhh},{key}): {e:?}");
                Err(VmInternalError::DBError(SQL_FAIL_MESSAGE.into()).into())
            }
        }
    }

    pub fn has_entry(conn: &Connection, key: &str) -> Result<bool, VmExecutionError> {
        sqlite_has_entry(conn, key)
    }
}

impl SqliteConnection {
    pub fn initialize_conn(conn: &Connection) -> Result<(), VmExecutionError> {
        conn.query_row("PRAGMA journal_mode = WAL;", NO_PARAMS, |_row| Ok(()))
            .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS data_table
                      (key TEXT PRIMARY KEY, value TEXT)",
            NO_PARAMS,
        )
        .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS metadata_table
                      (key TEXT NOT NULL, blockhash TEXT, value TEXT,
                       UNIQUE (key, blockhash))",
            NO_PARAMS,
        )
        .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS md_blockhashes ON metadata_table(blockhash)",
            NO_PARAMS,
        )
        .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;

        Self::check_schema(conn)?;

        Ok(())
    }

    pub fn memory() -> Result<Connection, VmExecutionError> {
        let contract_db = SqliteConnection::inner_open(":memory:")?;
        SqliteConnection::initialize_conn(&contract_db)?;
        Ok(contract_db)
    }

    pub fn check_schema(conn: &Connection) -> Result<(), VmExecutionError> {
        let sql = "SELECT sql FROM sqlite_master WHERE name=?";
        let _: String = conn
            .query_row(sql, params!["data_table"], |row| row.get(0))
            .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;
        let _: String = conn
            .query_row(sql, params!["metadata_table"], |row| row.get(0))
            .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;
        let _: String = conn
            .query_row(sql, params!["md_blockhashes"], |row| row.get(0))
            .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;
        Ok(())
    }

    fn inner_open(filename: &str) -> Result<Connection, VmExecutionError> {
        let conn = Connection::open(filename)
            .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;

        conn.busy_handler(Some(tx_busy_handler))
            .map_err(|x| VmInternalError::SqliteError(IncomparableError { err: x }))?;

        Ok(conn)
    }
}

pub struct MemoryBackingStore {
    side_store: Connection,
    /// Instrumentation for tests/benchmarks; these are backing-store calls, not
    /// physical I/O counts or a substitute for measuring the actual MARF.
    #[cfg(any(test, feature = "testing"))]
    pub metadata_resolutions: usize,
    #[cfg(any(test, feature = "testing"))]
    pub metadata_batches: usize,
    #[cfg(any(test, feature = "testing"))]
    pub data_reads: usize,
    #[cfg(any(test, feature = "testing"))]
    pub block_height_reads: usize,
}

impl Default for MemoryBackingStore {
    fn default() -> Self {
        MemoryBackingStore::new()
    }
}

impl MemoryBackingStore {
    #[allow(clippy::unwrap_used)]
    pub fn new() -> MemoryBackingStore {
        let side_store = SqliteConnection::memory().unwrap();

        let mut memory_marf = MemoryBackingStore {
            side_store,
            #[cfg(any(test, feature = "testing"))]
            metadata_resolutions: 0,
            #[cfg(any(test, feature = "testing"))]
            metadata_batches: 0,
            #[cfg(any(test, feature = "testing"))]
            data_reads: 0,
            #[cfg(any(test, feature = "testing"))]
            block_height_reads: 0,
        };

        memory_marf.as_clarity_db().initialize();

        memory_marf
    }

    pub fn as_clarity_db(&mut self) -> ClarityDatabase<'_> {
        ClarityDatabase::new(self, &NULL_HEADER_DB, &NULL_BURN_STATE_DB)
    }

    pub fn as_analysis_db(&mut self) -> AnalysisDatabase<'_> {
        AnalysisDatabase::new(self)
    }
}

impl ClarityBackingStore for MemoryBackingStore {
    fn set_block_hash(&mut self, bhh: StacksBlockId) -> Result<StacksBlockId, VmExecutionError> {
        Err(RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0)).into())
    }

    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError> {
        #[cfg(any(test, feature = "testing"))]
        {
            self.data_reads += 1;
        }
        SqliteConnection::get(self.get_side_store(), key)
    }

    fn get_data_from_path(&mut self, hash: &TrieHash) -> Result<Option<String>, VmExecutionError> {
        SqliteConnection::get(self.get_side_store(), hash.to_string().as_str())
    }

    fn get_data_with_proof(
        &mut self,
        key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        Ok(SqliteConnection::get(self.get_side_store(), key)?.map(|x| (x, vec![])))
    }

    fn get_data_with_proof_from_path(
        &mut self,
        hash: &TrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        self.get_data_with_proof(&hash.to_string())
    }

    fn get_side_store(&mut self) -> &Connection {
        &self.side_store
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        #[cfg(any(test, feature = "testing"))]
        {
            self.block_height_reads += 1;
        }
        if height == 0 {
            Some(StacksBlockId([255; 32]))
        } else {
            None
        }
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        StacksBlockId([255; 32])
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        0
    }

    fn get_current_block_height(&mut self) -> u32 {
        1
    }

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        None
    }

    fn put_all_data(&mut self, items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        for (key, value) in items.into_iter() {
            SqliteConnection::put(self.get_side_store(), &key, &value)?;
        }
        Ok(())
    }

    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        #[cfg(any(test, feature = "testing"))]
        {
            self.metadata_resolutions += 1;
        }
        sqlite_get_contract_hash(self, contract)
    }

    fn insert_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> Result<(), VmExecutionError> {
        sqlite_insert_metadata(self, contract, key, value)
    }

    fn insert_metadata_value(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &crate::vm::database::clarity_store::MetadataValue,
    ) -> Result<(), VmExecutionError> {
        crate::vm::database::sqlite::sqlite_insert_metadata_value(self, contract, key, value)
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        sqlite_get_metadata(self, contract, key)
    }

    fn get_metadata_batch(
        &mut self,
        contract: &QualifiedContractIdentifier,
        keys: &[String],
        deployment: &mut Option<StacksBlockId>,
    ) -> Result<Vec<Option<crate::vm::database::clarity_store::MetadataValue>>, VmExecutionError>
    {
        #[cfg(any(test, feature = "testing"))]
        {
            if !keys.is_empty() {
                self.metadata_batches += 1;
            }
        }
        sqlite_get_metadata_batch(self, contract, keys, deployment)
    }

    fn get_metadata_manual(
        &mut self,
        at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        sqlite_get_metadata_manual(self, at_height, contract, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_batch_preserves_order_absent_keys_and_duplicates() {
        let mut store = MemoryBackingStore::new();
        let id = QualifiedContractIdentifier::local("batch").unwrap();
        let mut db = store.as_clarity_db();
        db.begin();
        db.insert_contract_hash(&id, "source").unwrap();
        db.set_metadata(&id, "a", "first").unwrap();
        db.set_metadata(&id, "b", "second").unwrap();
        db.commit().unwrap();
        db.begin();
        db.set_metadata(&id, "a", "pending").unwrap();
        let keys: Vec<_> = ["b", "absent", "a", "b"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            db.store
                .get_metadata_batch(&id, &keys, &mut None)
                .unwrap()
                .into_iter()
                .map(|v| v.map(|v| v.into_text().unwrap()))
                .collect::<Vec<_>>(),
            vec![
                Some("second".into()),
                None,
                Some("pending".into()),
                Some("second".into())
            ]
        );
        db.roll_back().unwrap();
    }

    #[test]
    fn trigger_bad_block_height() {
        let mut store = MemoryBackingStore::default();
        let contract_id = QualifiedContractIdentifier::transient();
        // Use a block height that does NOT exist in MemoryBackingStore
        // MemoryBackingStore::get_block_at_height returns None for any height != 0
        let nonexistent_height = 42;
        let key = "some-metadata-key";

        let err = sqlite_get_metadata_manual(&mut store, nonexistent_height, &contract_id, key)
            .unwrap_err();

        assert!(
            matches!(
                err,
                VmExecutionError::Runtime(RuntimeError::BadBlockHeight(_), _)
            ),
            "Expected BadBlockHeight. Got {err}"
        );
    }

    #[test]
    fn metadata_copy_preserves_binary_and_text_types() {
        let source = SqliteConnection::memory().unwrap();
        let destination = SqliteConnection::memory().unwrap();
        source
            .execute(
                "INSERT INTO metadata_table VALUES ('text','block','json')",
                [],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO metadata_table VALUES ('binary','block',?1)",
                [vec![0u8, 255, 42]],
            )
            .unwrap();
        SqliteConnection::visit_metadata_rows(&source, |row| {
            SqliteConnection::insert_metadata_row(&destination, row)
        })
        .unwrap();
        let (kind, bytes): (String, Vec<u8>) = destination
            .query_row(
                "SELECT typeof(value),value FROM metadata_table WHERE key='binary'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "blob");
        assert_eq!(bytes, vec![0, 255, 42]);
        let (kind, value): (String, String) = destination
            .query_row(
                "SELECT typeof(value),value FROM metadata_table WHERE key='text'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "text");
        assert_eq!(value, "json");
    }

    #[test]
    fn metadata_keys_visited_in_order() {
        let conn = SqliteConnection::memory().unwrap();
        let block_id = StacksBlockId([0x11; 32]).to_hex();

        // Insert keys out of order; visit_metadata_keys must yield them sorted.
        for key in [
            "clr-meta::contract-z::source",
            "clr-meta::contract-a::source",
            "clr-meta::contract-m::source",
            "clr-meta::contract-a::other",
        ] {
            SqliteConnection::insert_metadata_row(
                &conn,
                &MetadataRow {
                    key,
                    block_id: &block_id,
                    value: "v".into(),
                },
            )
            .unwrap();
        }

        let mut visited: Vec<String> = Vec::new();
        SqliteConnection::visit_metadata_keys(&conn, |key| -> Result<(), rusqlite::Error> {
            visited.push(key.to_string());
            Ok(())
        })
        .unwrap();

        let mut sorted = visited.clone();
        sorted.sort();
        assert_eq!(
            visited, sorted,
            "visit_metadata_keys must yield keys in ascending order"
        );
        assert_eq!(visited.len(), 4, "all rows must be visited");
    }

    #[test]
    fn metadata_key_make_and_parse() {
        // Round-trips through the `clr-meta::<contract id>::<key>` format.
        let key = SqliteConnection::make_metadata_key("ST000.contract", "var");
        assert_eq!(key, "clr-meta::ST000.contract::var");
        assert_eq!(
            SqliteConnection::parse_metadata_key(&key),
            Some(("ST000.contract", "var"))
        );

        // The metadata key may itself contain "::"; only the first separator
        // after the prefix is the boundary (contract ids never contain "::").
        let key = SqliteConnection::make_metadata_key("ST000.contract", "vm-metadata::9::sub");
        assert_eq!(
            SqliteConnection::parse_metadata_key(&key),
            Some(("ST000.contract", "vm-metadata::9::sub"))
        );

        // An empty metadata key is still well-formed.
        let key = SqliteConnection::make_metadata_key("ST000.contract", "");
        assert_eq!(
            SqliteConnection::parse_metadata_key(&key),
            Some(("ST000.contract", ""))
        );

        // A realistic mainnet contract id round-trips.
        let key =
            SqliteConnection::make_metadata_key("SP000000000000000000002Q6VF78.pox-4", "vars");
        assert_eq!(
            SqliteConnection::parse_metadata_key(&key),
            Some(("SP000000000000000000002Q6VF78.pox-4", "vars"))
        );

        // The parser only checks shape, not identifiers: a key with an empty
        // contract id parses here (it's rejected later by contract-id parsing).
        assert_eq!(
            SqliteConnection::parse_metadata_key("clr-meta::::var"),
            Some(("", "var"))
        );

        // Keys outside the format (or with the prefix but no second separator)
        // do not parse.
        assert_eq!(
            SqliteConnection::parse_metadata_key("not-a-metadata-key"),
            None
        );
        assert_eq!(
            SqliteConnection::parse_metadata_key("clr-meta::no-second-separator"),
            None
        );
        assert_eq!(SqliteConnection::parse_metadata_key("clr-meta::"), None);
        assert_eq!(SqliteConnection::parse_metadata_key(""), None);
    }
}
