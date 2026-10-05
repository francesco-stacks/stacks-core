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

//! Disk startup, fork and restart coverage for executable metadata migration.
use clarity::vm::contexts::{ContractContext, OwnedEnvironment};
use clarity::vm::database::contract_storage::{CONTRACT_HEADER_KEY, LEGACY_CONTRACT_KEY};
use clarity::vm::database::{ClarityBackingStore, ClarityDatabase, ClaritySerializable};
use clarity::vm::test_util::{TEST_BURN_STATE_DB, TEST_HEADER_DB};
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value};
use rusqlite::{params, Connection};
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::types::StacksEpochId;

use crate::chainstate::stacks::index::ClarityMarfTrieId;
use crate::clarity_vm::clarity::{ClarityMarfStore, ClarityMarfStoreTransaction};
use crate::clarity_vm::database::marf::{ContractStorageMigration, MarfedKV};

fn deploy_value(db: ClarityDatabase, name: &str, value: u128) -> String {
    let id = QualifiedContractIdentifier::local(name).unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, StacksEpochId::Epoch41);
    env.initialize_versioned_contract(
        id.clone(),
        ClarityVersion::Clarity2,
        &format!("(define-read-only (value) u{value})"),
        None,
    )
    .unwrap();
    let (mut db, _) = env.destruct().unwrap();
    db.begin();
    let raw = db.get_serialized_contract(&id).unwrap().unwrap();
    db.roll_back().unwrap();
    raw
}

fn assert_value(db: ClarityDatabase, name: &str, value: u128) {
    let id = QualifiedContractIdentifier::local(name).unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, StacksEpochId::Epoch41);
    assert_eq!(
        env.execute_transaction(id.issuer.clone().into(), None, id, "value", &[])
            .unwrap()
            .0,
        Value::UInt(value)
    );
}

/// Replace only executable rows with the historical representation. Commitments,
/// heights, mutable data and the fork graph still come from real VM deployments.
fn restore_legacy(conn: &Connection, name: &str, block: &StacksBlockId, raw: &str) {
    let id = QualifiedContractIdentifier::local(name).unwrap();
    conn.execute(
        "DELETE FROM metadata_table WHERE blockhash=?1 AND (key=?2 OR key LIKE ?3)",
        params![
            block.to_string(),
            format!("clr-meta::{id}::{CONTRACT_HEADER_KEY}"),
            format!(
                "clr-meta::{id}::{}%",
                clarity::vm::database::contract_storage::FUNCTION_PREFIX
            )
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,?2,?3)",
        params![
            format!("clr-meta::{id}::{LEGACY_CONTRACT_KEY}"),
            block.to_string(),
            raw
        ],
    )
    .unwrap();
}

#[test]
fn explicit_node_migration_refuses_missing_and_unrelated_files_without_writes() {
    use crate::chainstate::stacks::db::StacksChainState;
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    assert!(StacksChainState::migrate_contract_storage(missing.to_str().unwrap()).is_err());
    assert!(!missing.exists());
    std::fs::create_dir_all(missing.join("vm")).unwrap();
    let path = missing.join("vm/index.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE unrelated(value TEXT)")
        .unwrap();
    drop(conn);
    let original = std::fs::read(&path).unwrap();
    assert!(StacksChainState::migrate_contract_storage(missing.to_str().unwrap()).is_err());
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(!missing.join("vm/clarity").exists());
}

#[test]
fn startup_migration_precedes_sortition_open_and_guards_interrupted_upgrades() {
    use clarity::vm::costs::ExecutionCost;

    use crate::burnchains::Burnchain;
    use crate::chainstate::burn::db::sortdb::SortitionDB;
    use crate::chainstate::coordinator::migrate_chainstate_dbs;
    use crate::chainstate::stacks::db::testing::TestChainstateBuilder;
    use crate::chainstate::stacks::db::StacksChainState;
    use crate::core::{StacksEpoch, PEER_VERSION_EPOCH_2_0};
    let chainstate = TestChainstateBuilder::new_testnet(function_name!()).build();
    let root = chainstate.root_path.clone();
    drop(chainstate);
    let dir = tempfile::tempdir().unwrap();
    let burnchain =
        Burnchain::new(dir.path().to_str().unwrap(), "bitcoin", "regtest", None).unwrap();
    let sortdb_path = dir.path().join("sortdb");
    let epochs = [StacksEpoch {
        epoch_id: StacksEpochId::Epoch20,
        start_height: 0,
        end_height: crate::core::STACKS_EPOCH_MAX,
        block_limit: ExecutionCost::max_value(),
        network_epoch: PEER_VERSION_EPOCH_2_0,
    }];
    drop(
        SortitionDB::connect(
            sortdb_path.to_str().unwrap(),
            burnchain.first_block_height,
            &burnchain.first_block_hash,
            u64::from(burnchain.first_block_timestamp),
            &epochs,
            burnchain.pox_constants.clone(),
            None,
            true,
            None,
        )
        .unwrap(),
    );
    let index = Connection::open(StacksChainState::header_index_root_path(
        root.clone().into(),
    ))
    .unwrap();
    index
        .execute("UPDATE db_config SET version='14'", [])
        .unwrap();
    drop(index);
    let conn = Connection::open(StacksChainState::vm_state_index_marf_path(
        root.clone().into(),
    ))
    .unwrap();
    conn.execute("DELETE FROM clarity_contract_storage", [])
        .unwrap();
    let id = QualifiedContractIdentifier::local("legacy-startup").unwrap();
    let key = format!("clr-meta::{id}::{LEGACY_CONTRACT_KEY}");
    conn.execute(
        "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,'legacy','broken')",
        params![key],
    )
    .unwrap();
    assert!(migrate_chainstate_dbs(
        &epochs,
        &burnchain,
        sortdb_path.to_str().unwrap(),
        &root,
        None
    )
    .is_err());
    assert_eq!(
        StacksChainState::get_db_config_from_path(&root)
            .unwrap()
            .version,
        "15",
        "the old binary must refuse even an interrupted conversion"
    );
    assert!(clarity::vm::database::contract_migration::check_contract_storage(&conn).is_err());
    let context = ContractContext::new(id, ClarityVersion::Clarity1);
    conn.execute(
        "UPDATE metadata_table SET value=?1 WHERE key=?2",
        params![context.serialize(), key],
    )
    .unwrap();
    drop(conn);
    migrate_chainstate_dbs(
        &epochs,
        &burnchain,
        sortdb_path.to_str().unwrap(),
        &root,
        None,
    )
    .unwrap();
    assert!(StacksChainState::open(false, CHAIN_ID_TESTNET, &root, None).is_ok());
    assert_eq!(
        StacksChainState::migrate_contract_storage(&root).unwrap(),
        0
    );
}

#[test]
fn ordinary_opens_refuse_legacy_storage_without_changing_chainstate() {
    use crate::chainstate::stacks::db::StacksChainState;

    let dir = tempfile::tempdir().unwrap();
    let vm_dir = StacksChainState::vm_state_index_root_path(dir.path().to_path_buf());
    let path = vm_dir.to_str().unwrap();
    let block = StacksBlockId([1; 32]);
    let mut kv = MarfedKV::open(path, None, None).unwrap();
    let mut store = kv.begin(&StacksBlockId::sentinel(), &block);
    let raw = deploy_value(
        store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
        "legacy",
        7,
    );
    store.commit_to_processed_block(&block).unwrap();
    drop(kv);
    let db_path = vm_dir.join("marf.sqlite");
    let conn = Connection::open(&db_path).unwrap();
    restore_legacy(&conn, "legacy", &block, &raw);
    conn.execute_batch("DROP TABLE clarity_contract_storage")
        .unwrap();
    drop(conn);
    let before = std::fs::read(&db_path).unwrap();

    let error = MarfedKV::open(path, None, None).err().unwrap();
    assert!(error.to_string().contains("require migration"));
    assert!(MarfedKV::open_unconfirmed(path, None, None).is_err());
    assert!(
        StacksChainState::open(false, CHAIN_ID_TESTNET, dir.path().to_str().unwrap(), None)
            .is_err()
    );
    assert_eq!(std::fs::read(&db_path).unwrap(), before);
    assert!(!StacksChainState::blocks_path(dir.path().to_path_buf()).exists());
    assert!(!StacksChainState::header_index_root_path(dir.path().to_path_buf()).exists());
    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='clarity_contract_storage'",
            [],
            |r| r.get::<_, u64>(0),
        )
        .unwrap(),
        0
    );
    drop(conn);

    // An unconfirmed opener can explicitly migrate without a preceding confirmed open.
    let mut kv = MarfedKV::open_unconfirmed_with_migration(
        path,
        None,
        None,
        ContractStorageMigration::Allow,
    )
    .unwrap();
    let mut store = kv.begin_read_only(Some(&block));
    assert_value(
        store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
        "legacy",
        7,
    );
    drop(kv);
    assert!(MarfedKV::open(path, None, None).is_ok());
    assert!(MarfedKV::open_unconfirmed(path, None, None).is_ok());
}

#[test]
fn ordinary_opens_refuse_incomplete_and_unsupported_storage_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    drop(MarfedKV::open(path, None, None).unwrap());
    let db_path = dir.path().join("marf.sqlite");
    for statement in [
        "DELETE FROM clarity_contract_storage",
        "INSERT INTO clarity_contract_storage VALUES(1,999)",
        "CREATE TABLE contract_storage_format(version INTEGER)",
    ] {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(statement).unwrap();
        drop(conn);
        let before = std::fs::read(&db_path).unwrap();
        assert!(MarfedKV::open(path, None, None).is_err());
        assert!(MarfedKV::open_unconfirmed(path, None, None).is_err());
        assert_eq!(std::fs::read(&db_path).unwrap(), before);
    }
}

#[test]
fn startup_migration_resumes_and_executes_forks_and_unconfirmed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let parent = StacksBlockId([1; 32]);
    let forks = [StacksBlockId([2; 32]), StacksBlockId([3; 32])];
    let mut kv = MarfedKV::open(path, None, None).unwrap();
    kv.begin(&StacksBlockId::sentinel(), &parent)
        .commit_to_processed_block(&parent)
        .unwrap();
    let mut originals = vec![];
    for (i, block) in forks.iter().enumerate() {
        let mut store = kv.begin(&parent, block);
        originals.push(deploy_value(
            store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
            "forked",
            i as u128 + 10,
        ));
        store.commit_to_processed_block(block).unwrap();
    }
    drop(kv);
    let mut kv = MarfedKV::open_unconfirmed(path, None, None).unwrap();
    let mut store = kv.begin_unconfirmed(&forks[0]);
    let raw = deploy_value(
        store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
        "unconfirmed",
        30,
    );
    let unconfirmed = store.get_open_chain_tip();
    store.commit_unconfirmed();
    drop(kv);

    let conn = Connection::open(dir.path().join("marf.sqlite")).unwrap();
    for (block, raw) in forks.iter().zip(&originals) {
        restore_legacy(&conn, "forked", block, raw);
    }
    restore_legacy(&conn, "unconfirmed", &unconfirmed, &raw);
    conn.execute("DELETE FROM clarity_contract_storage", [])
        .unwrap();
    // Force a later batch to fail after the first 256 contracts have committed.
    for i in 0..256 {
        let id = QualifiedContractIdentifier::local(&format!("a{i:03}")).unwrap();
        let context = ContractContext::new(id.clone(), ClarityVersion::Clarity2);
        conn.execute(
            "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,?2,?3)",
            params![
                format!("clr-meta::{id}::{LEGACY_CONTRACT_KEY}"),
                parent.to_string(),
                context.serialize()
            ],
        )
        .unwrap();
    }
    let bad = QualifiedContractIdentifier::local("zz-invalid").unwrap();
    let bad_key = format!("clr-meta::{bad}::{LEGACY_CONTRACT_KEY}");
    conn.execute(
        "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,?2,'broken')",
        params![bad_key, parent.to_string()],
    )
    .unwrap();
    assert!(
        MarfedKV::open_with_migration(path, None, None, ContractStorageMigration::Allow).is_err()
    );
    let converted: u64 = conn
        .query_row(
            "SELECT count(*) FROM metadata_table WHERE key LIKE '%::contract-header'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(converted, 256);
    let remaining: u64 = conn
        .query_row(
            "SELECT count(*) FROM metadata_table WHERE key LIKE '%::contract'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 4);
    conn.execute(
        "UPDATE metadata_table SET value=?1 WHERE key=?2",
        params![
            ContractContext::new(bad, ClarityVersion::Clarity2).serialize(),
            bad_key
        ],
    )
    .unwrap();
    drop(conn);

    let mut kv =
        MarfedKV::open_with_migration(path, None, None, ContractStorageMigration::Allow).unwrap();
    for (i, block) in forks.iter().enumerate() {
        let mut store = kv.begin_read_only(Some(block));
        assert_value(
            store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
            "forked",
            i as u128 + 10,
        );
    }
    drop(kv);
    let mut kv = MarfedKV::open_unconfirmed(path, None, None).unwrap();
    let mut store = kv.begin_read_only(Some(&unconfirmed));
    assert_value(
        store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
        "unconfirmed",
        30,
    );
    assert_value(
        store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
        "forked",
        10,
    );
}

#[test]
fn concurrent_startup_migrations_share_the_restart_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_owned();
    let block = StacksBlockId([1; 32]);
    let mut kv = MarfedKV::open(&path, None, None).unwrap();
    let mut store = kv.begin(&StacksBlockId::sentinel(), &block);
    let raw = deploy_value(
        store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
        "concurrent",
        7,
    );
    store.commit_to_processed_block(&block).unwrap();
    drop(kv);
    let conn = Connection::open(dir.path().join("marf.sqlite")).unwrap();
    restore_legacy(&conn, "concurrent", &block, &raw);
    for i in 0..512 {
        let id = QualifiedContractIdentifier::local(&format!("a{i:03}")).unwrap();
        conn.execute(
            "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,?2,?3)",
            params![
                format!("clr-meta::{id}::{LEGACY_CONTRACT_KEY}"),
                block.to_string(),
                ContractContext::new(id, ClarityVersion::Clarity2).serialize()
            ],
        )
        .unwrap();
    }
    conn.execute("DELETE FROM clarity_contract_storage", [])
        .unwrap();
    drop(conn);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let path = &path;
            let barrier = barrier.clone();
            let block = block.clone();
            scope.spawn(move || {
                barrier.wait();
                let mut kv = MarfedKV::open_with_migration(
                    path,
                    None,
                    None,
                    ContractStorageMigration::Allow,
                )
                .unwrap();
                let mut store = kv.begin_read_only(Some(&block));
                assert_value(
                    store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
                    "concurrent",
                    7,
                );
            });
        }
    });
    let conn = Connection::open(dir.path().join("marf.sqlite")).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM metadata_table WHERE key LIKE '%::contract'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
}

#[rstest::rstest]
#[case(StacksEpochId::Epoch33)]
#[case(StacksEpochId::Epoch34)]
#[case(StacksEpochId::Epoch40)]
#[case(StacksEpochId::Epoch41)]
fn boot_entrypoints_match_eager_loading_across_epochs_and_cache_bypass(
    #[case] epoch: StacksEpochId,
) {
    use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
    use clarity::vm::database::contract_codec::{decode_header, encode_header};
    use clarity::vm::database::sqlite::MemoryBackingStore;
    use clarity::vm::database::{ClarityExecutionCache, ContractCache};
    use clarity::vm::SymbolicExpression;

    let contracts = super::contract_storage_fixtures::boot_contracts();
    use super::contract_storage_fixtures::sample_argument;
    let mut comparisons = 0;
    let mut stores = [MemoryBackingStore::new(), MemoryBackingStore::new()];
    for store in &mut stores {
        let mut db = ClarityDatabase::new(store, &TEST_HEADER_DB, &TEST_BURN_STATE_DB);
        db.begin();
        db.set_clarity_epoch_version(epoch).unwrap();
        db.commit().unwrap();
        let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
        for (name, source, version) in contracts {
            env.initialize_versioned_contract(
                QualifiedContractIdentifier::local(name).unwrap(),
                version,
                source,
                None,
            )
            .unwrap();
        }
    }
    for (name, _, _) in contracts {
        let id = QualifiedContractIdentifier::local(name).unwrap();
        // Eager reference: force the production invocation path to load every
        // body, keeping charging, argument validation and call-stack handling
        // identical. No production test hook or alternate interpreter is used.
        let conn = stores[0].get_side_store();
        let key = format!("clr-meta::{id}::{CONTRACT_HEADER_KEY}");
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT value FROM metadata_table WHERE key=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        let mut header = decode_header(&bytes).unwrap();
        let all: Vec<_> = (0..header.functions.len() as u32).collect();
        for entry in &mut header.functions {
            entry.dependencies = all.clone();
        }
        conn.execute(
            "UPDATE metadata_table SET value=?1 WHERE key=?2",
            params![encode_header(&header).unwrap(), key],
        )
        .unwrap();
    }
    for (name, _, _) in contracts {
        let id = QualifiedContractIdentifier::local(name).unwrap();
        let mut db = stores[1].as_clarity_db();
        db.begin();
        let context = db.get_contract(&id).unwrap();
        db.roll_back().unwrap();
        drop(db);
        for (function, definition) in context.loaded_functions() {
            if !definition.is_public() {
                continue;
            }
            for nonzero in [false, true] {
                let args: Vec<_> = definition
                    .arg_types()
                    .iter()
                    .map(|ty| SymbolicExpression::atom_value(sample_argument(ty, nonzero)))
                    .collect();
                // Empty cache, then a budget which deliberately bypasses retention.
                for budget in [5 * 1024 * 1024, 1] {
                    let mut observations = vec![];
                    for store in &mut stores {
                        let mut cache = ClarityExecutionCache {
                            contracts: ContractCache::new(budget),
                        };
                        let mut db =
                            ClarityDatabase::new(store, &TEST_HEADER_DB, &TEST_BURN_STATE_DB)
                                .with_cache(&mut cache);
                        db.begin();
                        if budget == 1 {
                            db.get_contract(&id).unwrap();
                        }
                        let mut env = OwnedEnvironment::new_cost_limited(
                            false,
                            CHAIN_ID_TESTNET,
                            db,
                            LimitedCostTracker::new_with_limit(epoch, ExecutionCost::max_value()),
                            epoch,
                        );
                        let result = env.execute_transaction(
                            id.issuer.clone().into(),
                            None,
                            id.clone(),
                            function,
                            &args,
                        );
                        let cost = env.get_cost_total();
                        let (mut db, tracker) = env.destruct().unwrap();
                        let memory = tracker.get_memory();
                        db.roll_back().unwrap();
                        observations.push((result, cost, memory));
                    }
                    assert_eq!(
                        observations[0], observations[1],
                        "{epoch:?} {name}.{function}, nonzero={nonzero}, budget={budget}"
                    );
                    comparisons += 1;
                }
            }
        }
    }
    assert!(comparisons > 2500);
}

/// Exercise the production prepare-phase path with no .signers deployment.
#[rstest::rstest]
#[case(StacksEpochId::Epoch34)]
#[case(StacksEpochId::Epoch40)]
fn absent_signers_contract_skips_prepare_phase_update(#[case] epoch: StacksEpochId) {
    use crate::burnchains::PoxConstants;
    use crate::chainstate::nakamoto::signer_set::NakamotoSigners;
    use crate::chainstate::stacks::db::ClarityTx;
    use crate::clarity_vm::clarity::ClarityBlockConnection;

    let mut marf = MarfedKV::temporary();
    let mut tx = ClarityTx::from_block_connection(ClarityBlockConnection::new_test_conn(
        Box::new(marf.begin(&StacksBlockId::sentinel(), &StacksBlockId([3; 32]))),
        &TEST_HEADER_DB,
        &TEST_BURN_STATE_DB,
        epoch,
    ));
    let constants = PoxConstants::new(10, 5, 3, 25, 5, u64::MAX, u64::MAX, 0, 0, 0, 0, u32::MAX);
    assert!(constants.is_in_prepare_phase(0, 19));
    assert_eq!(constants.active_pox_contract_for_cycle(0, 2), "pox-4");
    assert!(
        NakamotoSigners::check_and_handle_prepare_phase_start(&mut tx, 0, &constants, 19, 1)
            .unwrap()
            .is_none()
    );
}
