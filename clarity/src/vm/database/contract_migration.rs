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

//! Restartable migration of executable metadata; consensus state is never rewritten.

use rusqlite::{Connection, OptionalExtension, params};

#[cfg(test)]
use super::ClaritySerializable;
use super::contract_storage::{
    CONTRACT_HEADER_KEY, LEGACY_CONTRACT_KEY, function_key, split_contract,
};
use super::{ClarityDeserializable, SqliteConnection};
use crate::vm::contexts::ContractContext;
use crate::vm::errors::{VmExecutionError, VmInternalError};

// Physical executable layout; independent of the consensus chainstate schema.
const STORAGE_VERSION: u32 = 1;
const BATCH_CONTRACTS: usize = 256;
const BATCH_SOURCE_BYTES: usize = 16 * 1024 * 1024;

fn failure(context: &str, error: impl std::fmt::Display) -> VmExecutionError {
    VmInternalError::DBError(format!("Contract storage migration {context}: {error}")).into()
}

/// Read the executable format marker without creating tables or changing records.
/// Missing markers also cover interrupted migrations, which must be resumed explicitly.
fn storage_version(conn: &Connection) -> Result<Option<u32>, VmExecutionError> {
    // Every experimental migration created its marker table before converting rows.
    // Checking the schema also detects interrupted experiments without scanning metadata.
    let experimental: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='contract_storage_format')",
        [], |row| row.get(0),
    ).map_err(|e| failure("checking experimental storage", e))?;
    if experimental {
        return Err(failure(
            "opening database",
            "unsupported experimental contract storage; restore an original snapshot",
        ));
    }
    let has_marker: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='clarity_contract_storage')",
        [], |row| row.get(0),
    ).map_err(|e| failure("checking format table", e))?;
    if !has_marker {
        return Ok(None);
    }
    let version: Option<u32> = conn
        .query_row(
            "SELECT version FROM clarity_contract_storage WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| failure("reading format", e))?;
    if let Some(version) = version
        && version != STORAGE_VERSION
    {
        return Err(failure(
            "opening database",
            format!("unsupported executable storage format {version}"),
        ));
    }
    Ok(version)
}

/// Require a completed migration, without modifying the supplied connection.
/// Inspection tools can call this before opening any writable chainstate handles.
pub fn check_contract_storage(conn: &Connection) -> Result<(), VmExecutionError> {
    if storage_version(conn)?.is_none() {
        return Err(failure(
            "opening database",
            "executable records require migration; start the node or explicitly run stacks-inspect migrate-contract-storage <chainstate-dir> on a disposable copy",
        ));
    }
    Ok(())
}

/// Convert legacy executable records, including forks and unconfirmed metadata.
/// Each bounded batch locks before scanning, then commits replacements and legacy
/// deletions atomically. Concurrent openers re-check the completion marker under
/// that lock and can safely resume the remaining legacy rows.
/// Remaining legacy rows are the restart cursor; the marker commits last.
pub fn migrate_contract_storage(conn: &Connection) -> Result<u64, VmExecutionError> {
    if storage_version(conn)?.is_some() {
        return Ok(0);
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clarity_contract_storage (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1), version INTEGER NOT NULL
    )",
    )
    .map_err(|e| failure("creating format table", e))?;
    let mut cursor = (String::new(), String::new());
    let mut migrated = 0u64;
    loop {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| failure("locking batch", e))?;
        let completed: Option<u32> = tx
            .query_row(
                "SELECT version FROM clarity_contract_storage WHERE singleton = 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| failure("rechecking format", e))?;
        if let Some(version) = completed {
            if version != STORAGE_VERSION {
                return Err(failure(
                    "opening database",
                    format!("unsupported executable storage format {version}"),
                ));
            }
            tx.commit()
                .map_err(|e| failure("finishing concurrent migration", e))?;
            return Ok(migrated);
        }
        let rows = {
            let mut query = tx
                .prepare_cached(
                    "SELECT key, blockhash, value FROM metadata_table
                 WHERE (key, blockhash) > (?1, ?2) AND key LIKE ?3
                 ORDER BY key, blockhash LIMIT ?4",
                )
                .map_err(|e| failure("preparing legacy scan", e))?;
            let mut query_rows = query
                .query(params![
                    cursor.0,
                    cursor.1,
                    format!("clr-meta::%::{LEGACY_CONTRACT_KEY}"),
                    BATCH_CONTRACTS
                ])
                .map_err(|e| failure("reading legacy rows", e))?;
            let mut rows = Vec::new();
            let mut bytes = 0usize;
            while let Some(row) = query_rows
                .next()
                .map_err(|e| failure("reading legacy row", e))?
            {
                let value: String = row.get(2).map_err(|e| failure("reading executable", e))?;
                bytes += value.len();
                rows.push((
                    row.get::<_, String>(0)
                        .map_err(|e| failure("reading key", e))?,
                    row.get::<_, String>(1)
                        .map_err(|e| failure("reading block", e))?,
                    value,
                ));
                if bytes >= BATCH_SOURCE_BYTES {
                    break;
                }
            }
            rows
        };
        if rows.is_empty() {
            tx.execute(
                "INSERT INTO clarity_contract_storage (singleton, version) VALUES (1, ?1)",
                [STORAGE_VERSION],
            )
            .map_err(|e| failure("writing format", e))?;
            tx.commit().map_err(|e| failure("committing format", e))?;
            break;
        }
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO metadata_table (key, blockhash, value) VALUES (?1, ?2, ?3)",
                )
                .map_err(|e| failure("preparing inserts", e))?;
            let mut delete = tx
                .prepare_cached("DELETE FROM metadata_table WHERE key = ?1 AND blockhash = ?2")
                .map_err(|e| failure("preparing deletions", e))?;
            for (key, blockhash, value) in rows {
                let (contract_id, metadata_key) = SqliteConnection::parse_metadata_key(&key)
                    .ok_or_else(|| failure(&key, "invalid metadata key"))?;
                if metadata_key != LEGACY_CONTRACT_KEY {
                    return Err(failure(&key, "unexpected legacy key"));
                }
                let legacy = ContractContext::deserialize(&value).map_err(|e| failure(&key, e))?;
                if legacy.contract_identifier.to_string() != contract_id {
                    return Err(failure(&key, "contract identity mismatch"));
                }
                let (header, functions) = split_contract(&legacy)?;
                header.validate_directory().map_err(|e| failure(&key, e))?;
                for (name, function) in functions {
                    insert
                        .execute(params![
                            format!("clr-meta::{contract_id}::{}", function_key(&name)),
                            blockhash,
                            super::contract_codec::encode_function(&function)?
                        ])
                        .map_err(|e| failure(&key, e))?;
                }
                insert
                    .execute(params![
                        format!("clr-meta::{contract_id}::{CONTRACT_HEADER_KEY}"),
                        blockhash,
                        super::contract_codec::encode_header(&header)?
                    ])
                    .map_err(|e| failure(&key, e))?;
                delete
                    .execute(params![key, blockhash])
                    .map_err(|e| failure(&key, e))?;
                cursor = (key, blockhash);
                migrated += 1;
            }
        }
        tx.commit().map_err(|e| failure("committing batch", e))?;
    }
    if migrated > 0 {
        info!("Executable contract migration complete"; "contracts" => migrated);
    }
    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::ClarityVersion;
    use crate::vm::callables::{DefineType, DefinedFunction};
    use crate::vm::representations::{ClarityName, SymbolicExpression};
    use crate::vm::types::{QualifiedContractIdentifier, Value};

    fn legacy(name: &str, value: u128) -> ContractContext {
        let id = QualifiedContractIdentifier::local(name).unwrap();
        let mut context = ContractContext::new(id.clone(), ClarityVersion::Clarity2);
        for name in ["a", "b"] {
            context.functions.insert(
                ClarityName::from_literal(name),
                DefinedFunction::new(
                    vec![],
                    SymbolicExpression::atom_value(Value::UInt(value)),
                    DefineType::Public,
                    &ClarityName::from_literal(name),
                    &id.to_string(),
                ),
            );
        }
        context
    }

    fn insert(conn: &Connection, context: &ContractContext, block: &str) -> String {
        let key = format!(
            "clr-meta::{}::{LEGACY_CONTRACT_KEY}",
            context.contract_identifier
        );
        conn.execute(
            "INSERT INTO metadata_table (key, blockhash, value) VALUES (?1, ?2, ?3)",
            params![key, block, context.serialize()],
        )
        .unwrap();
        key
    }

    #[test]
    fn migrates_each_fork_without_touching_consensus_data_and_is_idempotent() {
        let conn = SqliteConnection::memory().unwrap();
        let one = legacy("same", 1);
        let two = legacy("same", 2);
        insert(&conn, &one, "fork-a");
        insert(&conn, &two, "fork-b");
        conn.execute(
            "INSERT INTO data_table (key,value) VALUES ('commitment','untouched')",
            [],
        )
        .unwrap();
        assert_eq!(migrate_contract_storage(&conn).unwrap(), 2);
        assert_eq!(migrate_contract_storage(&conn).unwrap(), 0);
        for (block, expected) in [("fork-a", 1), ("fork-b", 2)] {
            let function = conn
                .query_row(
                    "SELECT value FROM metadata_table WHERE blockhash=?1 AND key=?2",
                    params![
                        block,
                        format!(
                            "clr-meta::{}::{}",
                            one.contract_identifier,
                            function_key("a")
                        )
                    ],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .unwrap();
            let function = super::super::contract_codec::decode_function(&function).unwrap();
            assert_eq!(
                function.body.match_atom_value(),
                Some(&Value::UInt(expected))
            );
        }
        let value: String = conn
            .query_row(
                "SELECT value FROM data_table WHERE key='commitment'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "untouched");
    }

    #[test]
    fn interruption_rolls_back_a_contract_and_retry_completes() {
        let conn = SqliteConnection::memory().unwrap();
        let context = legacy("interrupted", 1);
        let key = insert(&conn, &context, "block");
        conn.execute_batch(
            "CREATE TRIGGER interrupt_migration BEFORE INSERT ON metadata_table
            WHEN NEW.key LIKE '%contract-function::b'
            BEGIN SELECT RAISE(ABORT, 'simulated interruption'); END;",
        )
        .unwrap();
        assert!(migrate_contract_storage(&conn).is_err());
        let count: u64 = conn
            .query_row("SELECT COUNT(*) FROM metadata_table", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "no partial function records survive");
        let stored: String = conn
            .query_row(
                "SELECT value FROM metadata_table WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored).unwrap(),
            serde_json::from_str::<serde_json::Value>(&context.serialize()).unwrap()
        );
        conn.execute_batch("DROP TRIGGER interrupt_migration")
            .unwrap();
        assert_eq!(migrate_contract_storage(&conn).unwrap(), 1);
        let raw: Vec<u8> = conn
            .query_row(
                "SELECT value FROM metadata_table WHERE key=?1",
                params![format!(
                    "clr-meta::{}::{CONTRACT_HEADER_KEY}",
                    context.contract_identifier
                )],
                |r| r.get(0),
            )
            .unwrap();
        let header = super::super::contract_codec::decode_header(&raw).unwrap();
        assert_eq!(header.functions.len(), 2);
    }

    #[test]
    fn corruption_rolls_back_the_batch_and_does_not_mark_migration_complete() {
        let conn = SqliteConnection::memory().unwrap();
        let first = legacy("a-first", 1);
        let second = legacy("z-second", 2);
        insert(&conn, &first, "block");
        let bad_key = insert(&conn, &second, "block");
        conn.execute(
            "UPDATE metadata_table SET value='invalid' WHERE key=?1",
            params![bad_key],
        )
        .unwrap();
        let error = migrate_contract_storage(&conn).unwrap_err();
        assert!(format!("{error:?}").contains("z-second"));
        let markers: u64 = conn
            .query_row("SELECT COUNT(*) FROM clarity_contract_storage", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(markers, 0);
        conn.execute(
            "UPDATE metadata_table SET value=?1 WHERE key=?2",
            params![second.serialize(), bad_key],
        )
        .unwrap();
        assert_eq!(migrate_contract_storage(&conn).unwrap(), 2);
    }
    #[test]
    fn completed_batches_survive_failure_and_retry() {
        let conn = SqliteConnection::memory().unwrap();
        for index in 0..BATCH_CONTRACTS {
            insert(&conn, &legacy(&format!("a-{index:04}"), 1), "fork");
        }
        let last = legacy("z-last", 2);
        let key = insert(&conn, &last, "fork");
        conn.execute(
            "UPDATE metadata_table SET value='corrupt' WHERE key=?1",
            [&key],
        )
        .unwrap();
        assert!(migrate_contract_storage(&conn).is_err());
        let remaining: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata_table WHERE key LIKE '%::contract'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1);
        conn.execute(
            "UPDATE metadata_table SET value=?1 WHERE key=?2",
            params![last.serialize(), key],
        )
        .unwrap();
        assert_eq!(migrate_contract_storage(&conn).unwrap(), 1);
        assert_eq!(migrate_contract_storage(&conn).unwrap(), 0);
    }

    #[test]
    fn rejects_experimental_migration_even_without_completion_marker() {
        let conn = SqliteConnection::memory().unwrap();
        let key = insert(&conn, &legacy("remaining", 7), "block");
        conn.execute_batch("CREATE TABLE contract_storage_format (singleton INTEGER PRIMARY KEY, version INTEGER NOT NULL)").unwrap();
        assert!(
            format!("{:?}", migrate_contract_storage(&conn).unwrap_err())
                .contains("unsupported experimental")
        );
        let remaining: i64 = conn
            .query_row(
                "SELECT count(*) FROM metadata_table WHERE key=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn rejects_unsupported_formats() {
        for version in [0, STORAGE_VERSION + 1, u32::MAX] {
            let conn = SqliteConnection::memory().unwrap();
            conn.execute_batch("CREATE TABLE clarity_contract_storage (singleton INTEGER PRIMARY KEY, version INTEGER);").unwrap();
            conn.execute(
                "INSERT INTO clarity_contract_storage VALUES (1, ?1)",
                [version],
            )
            .unwrap();
            assert!(
                format!("{:?}", migrate_contract_storage(&conn).unwrap_err())
                    .contains("unsupported executable storage format")
            );
        }
    }
}

#[cfg(test)]
mod historical_writer_tests {
    use super::*;
    use crate::vm::database::{ClarityBackingStore, MemoryBackingStore};
    use crate::vm::types::{QualifiedContractIdentifier, Value};

    #[test]
    fn migrates_original_json_writer_and_preserves_rpc_bytes() {
        // Produced by the unmodified writer at 3fd5d990, not by this branch's
        // legacy compatibility serializer. Regeneration lives in stackslib's
        // contract_storage_fixtures::export_original_contract_json.
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("contract_migration/legacy-head.json")).unwrap();
        let mut store = MemoryBackingStore::new();
        let conn = store.get_side_store();
        conn.execute_batch("DELETE FROM metadata_table; DELETE FROM data_table; DROP TABLE IF EXISTS clarity_contract_storage;").unwrap();
        for row in fixture["data"].as_array().unwrap() {
            conn.execute(
                "INSERT INTO data_table(key,value) VALUES(?1,?2)",
                params![row[0].as_str().unwrap(), row[1].as_str().unwrap()],
            )
            .unwrap();
        }
        let mut rpc = None;
        for row in fixture["metadata"].as_array().unwrap() {
            let key = row[0].as_str().unwrap();
            let value = row[2].as_str().unwrap();
            conn.execute(
                "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,?2,?3)",
                params![key, row[1].as_str().unwrap(), value],
            )
            .unwrap();
            if key.contains(".legacy-rpc::") && key.ends_with("::contract") {
                rpc = Some(value);
            }
        }
        assert!(
            rpc.is_some(),
            "original writer fixture must include its RPC record"
        );
        assert_eq!(migrate_contract_storage(conn).unwrap(), 2);
        let mut db = store.as_clarity_db();
        db.begin();
        let rich = db
            .get_contract(&QualifiedContractIdentifier::local("legacy-rich").unwrap())
            .unwrap();
        assert_eq!(
            rich.get_clarity_version(),
            &crate::vm::ClarityVersion::Clarity1
        );
        let Value::Tuple(extremes) = rich.lookup_variable("extremes").unwrap() else {
            panic!("missing extremes");
        };
        assert_eq!(extremes.get("signed").unwrap(), &Value::Int(i128::MIN));
        assert_eq!(extremes.get("unsigned").unwrap(), &Value::UInt(u128::MAX));
        assert_eq!(rich.defined_traits.len(), 1);
        assert_eq!(rich.implemented_traits.len(), 1);
        assert_eq!(rich.meta_data_map.len(), 1);
        assert_eq!(rich.meta_data_var.len(), 1);
        assert_eq!(rich.meta_ft.len(), 1);
        assert_eq!(rich.meta_nft.len(), 1);
        let actual = db
            .get_serialized_contract(&QualifiedContractIdentifier::local("legacy-rpc").unwrap())
            .unwrap()
            .unwrap();
        // Production fixtures have no developer diagnostics. The exact legacy
        // text checks field order as well as numeric values, types and bodies.
        #[cfg(not(feature = "developer-mode"))]
        assert_eq!(actual, rpc.unwrap());
        #[cfg(feature = "developer-mode")]
        assert!(actual.contains("contract_context"));
        db.roll_back().unwrap();
    }
}
