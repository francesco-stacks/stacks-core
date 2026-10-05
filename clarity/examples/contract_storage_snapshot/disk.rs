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

//! File-backed metadata measurements with verified SQLite page-cache misses.
//! Deployment and epoch are supplied by the fixture: no MARF or execution timing.

use clarity::vm::contracts::Contract;
use clarity::vm::database::clarity_store::{MetadataValue, NullBackingStore};
use clarity::vm::database::sqlite::{sqlite_get_metadata, sqlite_get_metadata_batch};
use clarity::vm::database::{ClarityBackingStore, NULL_BURN_STATE_DB, NULL_HEADER_DB};
use clarity::vm::errors::VmExecutionError;
use stacks_common::types::chainstate::{StacksBlockId, TrieHash};

use super::*;

/// Read-only metadata store. Unsupported state operations panic via NullBackingStore.
struct DiskStore {
    connection: Connection,
    deployment: StacksBlockId,
}

impl DiskStore {
    fn open(path: &str) -> Result<Self> {
        let connection = read_only(path)?;
        // mmap bypasses SQLite's page cache and would invalidate the cold-page comparison.
        connection.execute_batch("PRAGMA mmap_size=0; PRAGMA cache_size=-2000;")?;
        Ok(Self {
            connection,
            deployment: StacksBlockId([0; 32]),
        })
    }

    fn database(&mut self) -> ClarityDatabase<'_> {
        ClarityDatabase::new(self, &NULL_HEADER_DB, &NULL_BURN_STATE_DB)
    }

    /// Read/reset a SQLite connection counter without touching database pages.
    fn page_misses(&self, reset: bool) -> i32 {
        let (mut current, mut highwater) = (0, 0);
        // SAFETY: this live, exclusively used connection owns the handle; both
        // output pointers are valid and SQLite retains neither of them.
        let status = unsafe {
            rusqlite::ffi::sqlite3_db_status(
                self.connection.handle(),
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
                &mut current,
                &mut highwater,
                i32::from(reset),
            )
        };
        assert_eq!(status, rusqlite::ffi::SQLITE_OK);
        current
    }
}

// Keep the fixture deliberately read-only and fail on unexpected VM operations.
macro_rules! unsupported {
    ($(fn $name:ident(&mut self $(, $arg:ident: $ty:ty)*) -> $ret:ty;)+) => {
        $(fn $name(&mut self $(, $arg: $ty)*) -> $ret {
            NullBackingStore {}.$name($($arg),*)
        })+
    };
}

impl ClarityBackingStore for DiskStore {
    unsupported! {
        fn put_all_data(&mut self, items: Vec<(String, String)>) -> std::result::Result<(), VmExecutionError>;
        fn get_data_from_path(&mut self, hash: &TrieHash) -> std::result::Result<Option<String>, VmExecutionError>;
        fn get_data_with_proof(&mut self, key: &str) -> std::result::Result<Option<(String, Vec<u8>)>, VmExecutionError>;
        fn get_data_with_proof_from_path(&mut self, hash: &TrieHash) -> std::result::Result<Option<(String, Vec<u8>)>, VmExecutionError>;
        fn set_block_hash(&mut self, block: StacksBlockId) -> std::result::Result<StacksBlockId, VmExecutionError>;
        fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId>;
        fn get_current_block_height(&mut self) -> u32;
        fn get_open_chain_tip_height(&mut self) -> u32;
        fn get_open_chain_tip(&mut self) -> StacksBlockId;
        fn insert_metadata(&mut self, id: &QualifiedContractIdentifier, key: &str, value: &str) -> std::result::Result<(), VmExecutionError>;
        fn get_metadata_manual(&mut self, height: u32, id: &QualifiedContractIdentifier, key: &str) -> std::result::Result<Option<String>, VmExecutionError>;
    }

    fn get_data(&mut self, key: &str) -> std::result::Result<Option<String>, VmExecutionError> {
        assert_eq!(key, "vm-epoch::epoch-version");
        Ok(Some(ClaritySerializable::serialize(
            &(StacksEpochId::Epoch40 as u32),
        )))
    }

    fn get_side_store(&mut self) -> &Connection {
        &self.connection
    }

    fn get_contract_hash(
        &mut self,
        _id: &QualifiedContractIdentifier,
    ) -> std::result::Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        Ok((self.deployment.clone(), Sha512Trunc256Sum([0; 32])))
    }

    fn insert_metadata_value(
        &mut self,
        _: &QualifiedContractIdentifier,
        _: &str,
        _: &MetadataValue,
    ) -> std::result::Result<(), VmExecutionError> {
        panic!("Disk benchmark store is read-only")
    }

    fn get_metadata(
        &mut self,
        id: &QualifiedContractIdentifier,
        key: &str,
    ) -> std::result::Result<Option<String>, VmExecutionError> {
        sqlite_get_metadata(self, id, key)
    }

    fn get_metadata_batch(
        &mut self,
        id: &QualifiedContractIdentifier,
        keys: &[String],
        deployment: &mut Option<StacksBlockId>,
    ) -> std::result::Result<Vec<Option<MetadataValue>>, VmExecutionError> {
        sqlite_get_metadata_batch(self, id, keys, deployment)
    }
}

/// Historical three point reads and full JSON decode, using current VM types.
/// Canonicalization uses the current implementation for both formats, making
/// this a format/load comparison, not a benchmark of the unchanged node binary.
fn load(
    store: &mut DiskStore,
    id: &QualifiedContractIdentifier,
    name: &str,
    split: bool,
) -> Contract {
    let mut db = store.database();
    db.begin();
    let contract = if split {
        db.get_contract_for_function(id, name).unwrap()
    } else {
        let raw = db
            .store
            .get_metadata(id, LEGACY_CONTRACT_KEY)
            .unwrap()
            .unwrap();
        let mut context = <ContractContext as ClarityDeserializable<_>>::deserialize(&raw).unwrap();
        context
            .canonicalize_types(&db.get_clarity_epoch_version().unwrap())
            .unwrap();
        // Do not use today's batched get_contract_size for the legacy control.
        for key in [
            "vm-metadata::9::contract-size",
            "vm-metadata::9::contract-data-size",
        ] {
            let raw = db.store.get_metadata(id, key).unwrap().unwrap();
            black_box(<u64 as ClarityDeserializable<_>>::deserialize(&raw).unwrap());
        }
        context.into()
    };
    db.roll_back().unwrap();
    contract
}

/// Measure the same deployment/entrypoint sample as `bench` against real files.
/// Each timing includes construction and destruction, excludes cache reset,
/// and has no VM cache. Alternate formats and cache modes to reduce order bias.
pub(super) fn bench(legacy: &str, migrated: &str, sample: &str, output: &str) -> Result<()> {
    let sample: Vec<serde_json::Value> = serde_json::from_reader(File::open(sample)?)?;
    let mut stores = [DiskStore::open(legacy)?, DiskStore::open(migrated)?];
    let mut results = Vec::new();
    for entry in sample {
        let id = QualifiedContractIdentifier::parse(entry["contract"].as_str().ok_or("contract")?)?;
        let name = entry["entrypoint"].as_str().ok_or("entrypoint")?;
        let deployment = StacksBlockId::from_hex(entry["blockhash"].as_str().ok_or("blockhash")?)?;
        for store in &mut stores {
            store.deployment = deployment.clone();
        }
        let original = load(&mut stores[0], &id, name, false);
        let candidate = load(&mut stores[1], &id, name, true);
        assert_eq!(
            candidate.loaded_functions().len() as u64,
            entry["loaded_functions"].as_u64().unwrap()
        );
        for (function_name, definition) in candidate.loaded_functions() {
            assert_eq!(
                ClaritySerializable::serialize(&FunctionDefinition::from(definition)),
                ClaritySerializable::serialize(&FunctionDefinition::from(
                    original.lookup_function(function_name).unwrap()
                )),
            );
        }
        drop((original, candidate));
        let mut times: [Vec<u128>; 4] = std::array::from_fn(|_| Vec::new());
        let mut misses: [Vec<i32>; 4] = std::array::from_fn(|_| Vec::new());
        for round in 0..40 {
            let order = [[0, 1, 2, 3], [1, 0, 3, 2], [2, 3, 0, 1], [3, 2, 1, 0]];
            for case in order[round % order.len()] {
                let split = case % 2 == 1;
                let cold = case >= 2;
                let store = &mut stores[usize::from(split)];
                if cold {
                    store.connection.execute_batch("PRAGMA shrink_memory")?;
                } else {
                    drop(black_box(load(store, &id, name, split)));
                }
                store.page_misses(true);
                let start = Instant::now();
                drop(black_box(load(store, &id, name, split)));
                times[case].push(start.elapsed().as_nanos());
                let count = store.page_misses(false);
                if cold {
                    assert!(count > 0, "cold load did not miss SQLite's page cache");
                }
                misses[case].push(count);
            }
        }
        let mut row = entry;
        for (case, label) in [
            "json_primed",
            "postcard_primed",
            "json_page_cold",
            "postcard_page_cold",
        ]
        .iter()
        .enumerate()
        {
            times[case].sort_unstable();
            misses[case].sort_unstable();
            row[*label] = serde_json::json!({
                "median_ns": (times[case][19] + times[case][20]) / 2,
                "p10_ns": times[case][4], "p90_ns": times[case][35],
                "median_sqlite_page_misses": (misses[case][19] + misses[case][20]) / 2,
            });
        }
        // Remove timings from the older, in-memory fixture to prevent confusion.
        for key in [
            "eager_load_ns",
            "split_load_ns",
            "split_warm_ns",
            "eager_retained_heap",
            "split_retained_heap",
        ] {
            row.as_object_mut().unwrap().remove(key);
        }
        eprintln!("disk benchmark {id}.{}", row["entrypoint"]);
        results.push(row);
    }
    write_json(
        output,
        &serde_json::json!({
            "vm_cache": "absent", "epoch": "4.0", "mmap_size": 0,
            "sqlite_cache_kib": 2000, "samples_per_case": 40,
        "sqlite_version": rusqlite::version(),
        "sqlite_page_bytes": stores[0].connection.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?,
            "cold_protocol": "PRAGMA shrink_memory before each timed load; positive CACHE_MISS asserted",
            "os_cache": "uncontrolled; filesystem-cold I/O is NOT measured",
            "excluded": ["MARF epoch/deployment resolution", "transaction execution", "consensus charging"],
            "baseline": "original full-contract JSON/three point reads, reconstructed with current VM types/canonicalization",
            "results": results,
        }),
    )
}
