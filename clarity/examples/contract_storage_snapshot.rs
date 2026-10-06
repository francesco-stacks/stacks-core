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

//! Audit, migrate, and measure an extracted executable-contract corpus.
//! Input databases are read-only except for the explicit `migrate` command.
//! See docs/clarity/contract-node-benchmark.md for preparation and interpretation.

#[path = "contract_storage_snapshot/disk.rs"]
mod disk;

use std::alloc::System;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::File;
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use clarity::vm::callables::FunctionDefinition;
use clarity::vm::contexts::ContractContext;
use clarity::vm::database::contract_migration::migrate_contract_storage;
use clarity::vm::database::contract_storage::{
    CONTRACT_HEADER_KEY, ContractHeader, LEGACY_CONTRACT_KEY, split_contract,
};
use clarity::vm::database::{
    ClarityDatabase, ClarityDeserializable, ClarityExecutionCache, ClaritySerializable,
    MemoryBackingStore, SqliteConnection,
};
use clarity::vm::types::QualifiedContractIdentifier;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use stacks_common::alloc_tracker::{TrackingAllocator, thread_allocated};
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::Sha512Trunc256Sum;

#[global_allocator]
static ALLOCATOR: TrackingAllocator<System> = TrackingAllocator { inner: System };

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Serialize, Deserialize)]
/// One deployed executable version, retained for deterministic sample selection.
struct InventoryEntry {
    /// Fully qualified legacy metadata key, including the contract identifier.
    key: String,
    /// Deployment block that disambiguates versions on different forks.
    blockhash: String,
    /// Length of the original executable JSON, excluding SQLite overhead.
    bytes: usize,
    /// Number of public, read-only, and private function definitions.
    functions: usize,
    /// Number of public and read-only entrypoints that can be sampled.
    callable_functions: usize,
}

fn read_only(path: &str) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

fn write_json(path: &str, value: &impl Serialize) -> Result<()> {
    let mut writer = BufWriter::new(File::options().write(true).create_new(true).open(path)?);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    Ok(())
}

/// Legacy HashSets have no stable array order. Compare those two set fields
/// independently of iteration order, retaining order everywhere else in the AST.
fn normalize_sets(mut value: serde_json::Value) -> serde_json::Value {
    for key in ["implemented_traits", "persisted_names"] {
        if let Some(values) = value.get_mut(key).and_then(serde_json::Value::as_array_mut) {
            values.sort_by_cached_key(|item| serde_json::to_string(item).unwrap());
        }
    }
    value
}

/// Extract executable records and their historical size metadata. Refuse to
/// overwrite an existing destination; source, analysis, and chainstate are omitted.
fn extract(path: &str, output: &str) -> Result<()> {
    let source = read_only(path)?;
    File::options().write(true).create_new(true).open(output)?;
    let mut destination = Connection::open(output)?;
    SqliteConnection::initialize_conn(&destination).map_err(|e| format!("{e:?}"))?;
    let mut statement = source.prepare(
        "SELECT key,blockhash,value FROM metadata_table
         WHERE key LIKE '%::vm-metadata::9::contract'
            OR key LIKE '%::vm-metadata::9::contract-size'
            OR key LIKE '%::vm-metadata::9::contract-data-size'
         ORDER BY key,blockhash",
    )?;
    let mut rows = statement.query([])?;
    let mut transaction = destination.transaction()?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        transaction.execute(
            "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,?2,?3)",
            params![
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?
            ],
        )?;
        count += 1;
        if count % 1000 == 0 {
            transaction.commit()?;
            transaction = destination.transaction()?;
        }
    }
    transaction.commit()?;
    destination.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    eprintln!(
        "extracted {count} metadata rows, {} file bytes",
        std::fs::metadata(output)?.len()
    );
    Ok(())
}

/// Decode and re-encode every contract through the current storage DTOs, then
/// compare all common data and function definitions against the legacy decoder.
fn audit(path: &str, output: &str) -> Result<()> {
    let db = read_only(path)?;
    let mut statement = db.prepare(
        "SELECT key, blockhash, value FROM metadata_table
         WHERE key LIKE '%::vm-metadata::9::contract' ORDER BY key, blockhash",
    )?;
    let mut rows = statement.query([])?;
    let start = Instant::now();
    let mut inventory = vec![];
    let mut header_bytes = 0u64;
    let mut function_bytes = 0u64;
    let mut certified = 0usize;
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let raw: String = row.get(2)?;
        let context = <ContractContext as ClarityDeserializable<_>>::deserialize(&raw)
            .map_err(|e| format!("{key}: {e:?}"))?;
        let (header, records) = split_contract(&context)?;
        let encoded_header = clarity::vm::database::contract_codec::encode_header(&header)?;
        header_bytes += encoded_header.len() as u64;
        certified += usize::from(header.constants_sanitized);
        let header = clarity::vm::database::contract_codec::decode_header(&encoded_header)?;
        let mut functions = BTreeMap::new();
        for (name, record) in records {
            let encoded = clarity::vm::database::contract_codec::encode_function(&record)?;
            function_bytes += encoded.len() as u64;
            let decoded = clarity::vm::database::contract_codec::decode_function(&encoded)?;
            functions.insert(name, decoded);
        }
        let mut rebuilt = serde_json::to_value(&header.shared)?;
        rebuilt
            .as_object_mut()
            .ok_or("shared record is not an object")?
            .insert("functions".into(), serde_json::to_value(functions)?);
        if normalize_sets(rebuilt) != normalize_sets(serde_json::to_value(&context)?) {
            return Err(format!("Round-trip mismatch: {key}").into());
        }
        inventory.push(InventoryEntry {
            key,
            blockhash: row.get(1)?,
            bytes: raw.len(),
            functions: context.loaded_functions().len(),
            callable_functions: context
                .loaded_functions()
                .values()
                .filter(|f| f.is_public())
                .count(),
        });
        if inventory.len() % 10_000 == 0 {
            eprintln!(
                "audited {} contracts in {:?}",
                inventory.len(),
                start.elapsed()
            );
        }
    }
    write_json(output, &inventory)?;
    eprintln!(
        "audit complete: {} contracts in {:?}; headers={header_bytes} bytes, functions={function_bytes} bytes, certified_constants={certified}",
        inventory.len(),
        start.elapsed()
    );
    Ok(())
}

/// Run the production migration on an explicitly provided disposable copy.
fn migrate(path: &str, output: &str) -> Result<()> {
    let headers = std::path::Path::new(path)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("index.sqlite"));
    if headers.is_some_and(|p| p.exists()) {
        return Err("Use stacks-inspect migrate-contract-storage on the chainstate directory for node databases".into());
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    // Match the node's SQLite configuration in util_lib/db.rs.
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    let before: u64 = db.query_row("SELECT COUNT(*) FROM metadata_table", [], |r| r.get(0))?;
    let bytes_before = std::fs::metadata(path)?.len();
    let start = Instant::now();
    let count = migrate_contract_storage(&db).map_err(|e| format!("Migration failed: {e:?}"))?;
    let seconds = start.elapsed().as_secs_f64();
    let repeated = migrate_contract_storage(&db).map_err(|e| format!("Retry failed: {e:?}"))?;
    if repeated != 0 {
        return Err("Migration was not idempotent".into());
    }
    let remaining: u64 = db.query_row(
        "SELECT COUNT(*) FROM metadata_table WHERE key LIKE '%::vm-metadata::9::contract'",
        [],
        |r| r.get(0),
    )?;
    let after: u64 = db.query_row("SELECT COUNT(*) FROM metadata_table", [], |r| r.get(0))?;
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let integrity: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" || remaining != 0 {
        return Err(format!("Post-migration check: {integrity}, {remaining} legacy rows").into());
    }
    write_json(
        output,
        &serde_json::json!({
            "converted": count, "migration_seconds": seconds, "metadata_rows_before": before,
            "metadata_rows_after": after, "file_bytes_before": bytes_before,
            "file_bytes_after_checkpoint": std::fs::metadata(path)?.len(), "quick_check": integrity,
            "retry_converted": repeated, "remaining_legacy_rows": remaining,
            "journal_mode": "WAL", "synchronous": "NORMAL",
        }),
    )?;
    eprintln!("migration complete: {count} contracts in {seconds:.3}s");
    Ok(())
}

/// Reproduce historical eager loading against the preserved legacy fixture,
/// including canonicalization and size reads, without executing an entrypoint.
fn eager_load(db: &mut ClarityDatabase, id: &QualifiedContractIdentifier) -> ContractContext {
    let raw = db
        .store
        .get_metadata(id, "benchmark-legacy")
        .unwrap()
        .unwrap();
    let mut context = <ContractContext as ClarityDeserializable<_>>::deserialize(&raw).unwrap();
    context
        .canonicalize_types(&db.get_clarity_epoch_version().unwrap())
        .unwrap();
    db.get_contract_size(id).unwrap();
    context
}

/// Median of independently timed batches; every iteration drops its returned
/// context. SQLite pages are warm, and the caller selects VM-cache residency.
fn median_ns<T>(mut f: impl FnMut() -> T) -> u128 {
    let mut samples = vec![];
    for _ in 0..7 {
        let start = Instant::now();
        let mut count = 0;
        while start.elapsed() < Duration::from_millis(20) {
            black_box(f());
            count += 1;
        }
        samples.push(start.elapsed().as_nanos() / count);
    }
    samples.sort_unstable();
    samples[3]
}

/// Count unique function bodies needed by an entrypoint, including itself.
fn closure_len(header: &ContractHeader, name: &str) -> usize {
    let mut seen = BTreeSet::new();
    let index = header
        .functions
        .binary_search_by(|entry| entry.name.as_str().cmp(name))
        .unwrap();
    let mut pending = vec![index];
    while let Some(index) = pending.pop() {
        if seen.insert(index) {
            pending.extend(
                header.functions[index]
                    .dependencies
                    .iter()
                    .map(|index| *index as usize),
            );
        }
    }
    seen.len()
}

/// Select size quantiles, the largest function directories, and deployed boot
/// contracts. Sample three closure sizes per contract without weighting by calls.
fn bench(legacy_path: &str, migrated_path: &str, inventory_path: &str, output: &str) -> Result<()> {
    let legacy_db = read_only(legacy_path)?;
    let migrated_db = read_only(migrated_path)?;
    let mut inventory: Vec<InventoryEntry> = serde_json::from_reader(File::open(inventory_path)?)?;
    inventory.retain(|row| row.callable_functions > 0);
    if inventory.is_empty() {
        return Err("Inventory contains no callable contracts".into());
    }
    inventory.sort_by_key(|row| (row.bytes, row.key.clone(), row.blockhash.clone()));
    let mut selected = BTreeSet::new();
    for percentile in [0, 10, 25, 50, 75, 90, 95, 99, 100] {
        selected.insert((inventory.len() - 1) * percentile / 100);
    }
    let mut by_functions: Vec<_> = (0..inventory.len()).collect();
    by_functions.sort_by_key(|i| inventory[*i].functions);
    selected.extend(by_functions.into_iter().rev().take(5));
    for (i, row) in inventory.iter().enumerate() {
        if row
            .key
            .starts_with("clr-meta::SP000000000000000000002Q6VF78.")
        {
            selected.insert(i);
        }
    }
    let mut results = vec![];
    for index in selected {
        let entry = &inventory[index];
        let raw: String = legacy_db.query_row(
            "SELECT value FROM metadata_table WHERE key=?1 AND blockhash=?2",
            params![entry.key, entry.blockhash],
            |r| r.get(0),
        )?;
        let context = <ContractContext as ClarityDeserializable<_>>::deserialize(&raw)
            .map_err(|e| format!("{e:?}"))?;
        let id = context.contract_identifier.clone();
        let header_key = format!("clr-meta::{id}::{CONTRACT_HEADER_KEY}");
        let encoded: Vec<u8> = migrated_db.query_row(
            "SELECT value FROM metadata_table WHERE key=?1 AND blockhash=?2",
            params![header_key, entry.blockhash],
            |r| r.get(0),
        )?;
        let header = clarity::vm::database::contract_codec::decode_header(&encoded)
            .map_err(|e| format!("{e:?}"))?;
        let mut methods: Vec<_> = context
            .loaded_functions()
            .iter()
            .filter(|(_, f)| f.is_public())
            .map(|(name, _)| (closure_len(&header, name), name.to_string()))
            .collect();
        methods.sort();
        let method_indices = BTreeSet::from([0, methods.len() / 2, methods.len() - 1]);
        let mut store = MemoryBackingStore::new();
        let mut db = store.as_clarity_db();
        db.begin();
        db.set_clarity_epoch_version(StacksEpochId::Epoch40)
            .unwrap();
        db.store
            .prepare_for_contract_metadata(
                &id,
                Sha512Trunc256Sum::from_data(b"snapshot benchmark fixture"),
            )
            .unwrap();
        let prefix = format!("clr-meta::{id}::");
        let mut query = migrated_db.prepare(
            "SELECT key,value FROM metadata_table INDEXED BY sqlite_autoindex_metadata_table_1
             WHERE blockhash=?1 AND key GLOB ?2",
        )?;
        for row in query.query_map(params![entry.blockhash, format!("{prefix}*")], |r| {
            let raw = r.get_ref(1)?;
            let value = match raw {
                rusqlite::types::ValueRef::Blob(bytes) => {
                    clarity::vm::database::clarity_store::MetadataValue::Blob(bytes.into())
                }
                _ => clarity::vm::database::clarity_store::MetadataValue::Text(
                    r.get::<_, String>(1)?,
                ),
            };
            Ok((r.get::<_, String>(0)?, value))
        })? {
            let (key, value) = row?;
            db.store
                .insert_metadata_value(
                    &id,
                    key.strip_prefix(&prefix)
                        .ok_or("unexpected metadata prefix")?,
                    value,
                )
                .unwrap();
        }
        db.set_metadata(&id, "benchmark-legacy", &raw).unwrap();
        db.commit().unwrap();
        db.begin();
        let restored = db.get_serialized_contract(&id).unwrap().unwrap();
        let restored =
            <ContractContext as ClarityDeserializable<_>>::deserialize(&restored).unwrap();
        if normalize_sets(serde_json::to_value(&context)?)
            != normalize_sets(serde_json::to_value(&restored)?)
        {
            return Err(format!("Stored migration mismatch: {id}").into());
        }
        let baseline = eager_load(&mut db, &id);
        let eager_ns = median_ns(|| eager_load(&mut db, &id));
        let before = thread_allocated();
        let allocated = eager_load(&mut db, &id);
        let eager_heap = thread_allocated().net_allocated(&before);
        drop(allocated);
        db.roll_back().unwrap();
        drop(db);
        for method_index in method_indices {
            let (selected_functions, name) = &methods[method_index];
            let mut db = store.as_clarity_db();
            db.begin();
            let before = thread_allocated();
            let partial = db.get_contract_for_function(&id, name).unwrap();
            let split_heap = thread_allocated().net_allocated(&before);
            assert_eq!(partial.loaded_functions().len(), *selected_functions);
            for (function_name, definition) in partial.loaded_functions() {
                assert_eq!(
                    ClaritySerializable::serialize(&FunctionDefinition::from(definition)),
                    ClaritySerializable::serialize(&FunctionDefinition::from(
                        baseline.lookup_function(function_name).unwrap()
                    ))
                );
            }
            for function_name in baseline.loaded_functions().keys() {
                assert!(partial.has_function(function_name));
            }
            drop(partial);
            let split_ns = median_ns(|| db.get_contract_for_function(&id, name).unwrap());
            db.roll_back().unwrap();
            drop(db);
            let mut cache = ClarityExecutionCache::default();
            let mut db = store.as_clarity_db().with_cache(&mut cache);
            db.begin();
            db.get_contract_for_function(&id, name).unwrap();
            let warm_ns = median_ns(|| db.get_contract_for_function(&id, name).unwrap());
            db.roll_back().unwrap();
            results.push(serde_json::json!({
                "function_encoding": "postcard",
                "contract": id.to_string(), "blockhash": entry.blockhash, "entrypoint": name,
                "legacy_bytes": entry.bytes, "total_functions": entry.functions,
                "loaded_functions": selected_functions, "eager_load_ns": eager_ns,
                "split_load_ns": split_ns, "split_warm_ns": warm_ns,
                "eager_retained_heap": eager_heap, "split_retained_heap": split_heap,
            }));
        }
        eprintln!(
            "benchmarked {id}: {} functions, {} bytes",
            entry.functions, entry.bytes
        );
    }
    write_json(output, &results)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [command, db, output] if command == "extract" => extract(db, output),
        [command, db, output] if command == "audit" => audit(db, output),
        [command, db, output] if command == "migrate" => migrate(db, output),
        [command, legacy, migrated, inventory, output] if command == "bench" => bench(legacy, migrated, inventory, output),
        [command, legacy, migrated, sample, output] if command == "bench-disk" => disk::bench(legacy, migrated, sample, output),
        _ => Err(format!("Usage: contract_storage_snapshot extract SOURCE_DB NEW_CORPUS_DB\n       contract_storage_snapshot audit|migrate DB REPORT.json\n       contract_storage_snapshot bench LEGACY_DB MIGRATED_DB INVENTORY.json REPORT.json\n       contract_storage_snapshot bench-disk LEGACY_DB MIGRATED_DB BENCH_SAMPLE.json REPORT.json\nLegacy metadata key: {LEGACY_CONTRACT_KEY}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("contract-corpus-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn report_cannot_overwrite_an_existing_input() {
        let dir = TempDir::new();
        let file = dir.0.join("snapshot");
        std::fs::write(&file, b"snapshot contents").unwrap();
        assert!(write_json(file.to_str().unwrap(), &vec![1u8]).is_err());
        assert_eq!(std::fs::read(file).unwrap(), b"snapshot contents");
    }

    #[test]
    fn empty_inventory_is_an_error() {
        let dir = TempDir::new();
        let database = dir.0.join("empty.sqlite");
        drop(Connection::open(&database).unwrap());
        let inventory = dir.0.join("inventory.json");
        std::fs::write(&inventory, b"[]").unwrap();
        let output = dir.0.join("report.json");
        let error = bench(
            database.to_str().unwrap(),
            database.to_str().unwrap(),
            inventory.to_str().unwrap(),
            output.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no callable contracts"));
        assert!(!output.exists());
    }
}
