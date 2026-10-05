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

use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::StacksEpochId;

use super::*;
use crate::vm::contexts::OwnedEnvironment;
use crate::vm::database::{ClarityExecutionCache, MemoryBackingStore};
use crate::vm::errors::RuntimeCheckErrorKind;

#[test]
fn every_native_name_matches_runtime_resolution_in_every_clarity_version() {
    use crate::vm::callables::CallableType;
    use crate::vm::{LocalContext, eval, lookup_function};
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    for version in ClarityVersion::ALL {
        for name in crate::vm::functions::NativeFunctions::ALL_NAMES {
            let native_name = ClarityName::from_literal(name);
            let entry = ClarityName::from_literal("entry");
            let call =
                SymbolicExpression::list(vec![SymbolicExpression::atom(native_name.clone())]);
            let mut context = ContractContext::new(id.clone(), *version);
            for (name, body) in [
                (
                    native_name.clone(),
                    SymbolicExpression::atom_value(Value::UInt(42)),
                ),
                (entry.clone(), call.clone()),
            ] {
                context.functions.insert(
                    name.clone(),
                    DefinedFunction::new(
                        vec![],
                        body,
                        DefineType::ReadOnly,
                        &name,
                        &id.to_string(),
                    ),
                );
            }
            let mut db = store.as_clarity_db();
            db.begin();
            db.insert_contract(&id, context.into()).unwrap();
            let full = db.get_contract(&id).unwrap();
            let selected = db.get_contract_for_function(&id, "entry").unwrap();
            let loaded_native = selected.functions.contains_key(&native_name);
            let mut env =
                OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, StacksEpochId::Epoch41);
            let mut outcomes = Vec::new();
            for context in [full, selected] {
                let outcome = env
                    .execute_in_env(
                        id.issuer.clone().into(),
                        None,
                        Some((*context).clone()),
                        |exec, invoke| {
                            let local = matches!(
                                lookup_function(name, exec, invoke)?,
                                CallableType::UserFunction(_)
                            );
                            assert_eq!(loaded_native, local, "{version:?}/{name}");
                            Ok::<_, VmExecutionError>(format!(
                                "{:?}",
                                eval(&call, exec, invoke, &LocalContext::new())
                            ))
                        },
                    )
                    .unwrap()
                    .0;
                outcomes.push(outcome);
            }
            assert_eq!(outcomes[0], outcomes[1], "{version:?}/{name}");
            let (mut db, _) = env.destruct().unwrap();
            db.roll_back().unwrap();
        }
    }
}

const SOURCE: &str = "
    (define-constant answer u42)
    (define-private (leaf (x uint)) (+ x answer))
    (define-private (unused) u99)
    (define-private (other) (leaf u1))
    (define-public (run) (ok (leaf u0)))
    (define-public (branches) (ok (if true (leaf u0) (other))))
    (define-public (mapped) (ok (map leaf (list u0 u1))))
    (define-public (tuple-key) (ok (get unused (tuple (unused (leaf u0))))))
    (define-public (bad-binding) (let ((unused u1)) (ok unused)))
";

#[test]
fn saving_a_selectively_loaded_context_is_rejected_before_writes() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    let mut db = store.as_clarity_db();
    db.begin();
    let original = db.get_serialized_contract(&id).unwrap().unwrap();
    let partial = db.get_contract_for_function(&id, "run").unwrap();
    assert!(partial.functions.len() < partial.function_names.len());
    assert!(db.insert_contract(&id, partial).is_err());
    // Compare contexts: HashMap order is not part of the historical JSON schema.
    let restored = db.get_serialized_contract(&id).unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&original).unwrap(),
        serde_json::from_str::<serde_json::Value>(&restored).unwrap()
    );
    db.roll_back().unwrap();
}

#[test]
fn pending_contract_counts_survive_nested_rollback_and_persisted_logs() {
    use crate::vm::database::{RollbackWrapper, RollbackWrapperPersistedLog};
    let mut store = MemoryBackingStore::new();
    let id = QualifiedContractIdentifier::local("pending").unwrap();
    let other = QualifiedContractIdentifier::local("other").unwrap();
    let mut db = RollbackWrapper::new(&mut store);
    db.nest();
    db.insert_metadata(&id, "a", "1").unwrap();
    db.nest();
    db.insert_metadata(&id, "a", "2").unwrap();
    db.insert_metadata(&other, "b", "3").unwrap();
    let log: RollbackWrapperPersistedLog = db.into();
    let mut db = RollbackWrapper::from_persisted_log(&mut store, log);
    assert!(db.has_pending_metadata_for_contract(&other));
    db.rollback().unwrap();
    assert!(!db.has_pending_metadata_for_contract(&other));
    assert!(db.has_pending_metadata_for_contract(&id));
    db.commit().unwrap();
    assert!(!db.has_pending_metadata_for_contract(&id));
}

fn deploy(store: &mut MemoryBackingStore, epoch: StacksEpochId) -> QualifiedContractIdentifier {
    let id = QualifiedContractIdentifier::local("split").unwrap();
    let mut db = store.as_clarity_db();
    db.begin();
    db.set_clarity_epoch_version(epoch).unwrap();
    db.commit().unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
    // Initialization without analysis deliberately retains the runtime namespace-error
    // fixture, which verifies checks do not depend on which bodies were loaded.
    env.initialize_versioned_contract(id.clone(), ClarityVersion::Clarity2, SOURCE, None)
        .unwrap();
    id
}

/// Existence does not interpret executable bytes, in pending or committed state.
/// Repeated contract-of calls must not decode large target headers.
#[rstest::rstest]
#[case(StacksEpochId::Epoch22)]
#[case(StacksEpochId::Epoch34)]
#[case(StacksEpochId::Epoch40)]
#[case(StacksEpochId::Epoch41)]
fn contract_of_uses_raw_presence_even_for_unreadable_large_header(#[case] epoch: StacksEpochId) {
    use super::super::clarity_store::ClarityBackingStore;
    let mut store = MemoryBackingStore::new();
    let mut db = store.as_clarity_db();
    db.begin();
    db.set_clarity_epoch_version(epoch).unwrap();
    db.commit().unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
    for (name, source) in [
        (
            "definition",
            "(define-trait t ((run () (response uint uint))))",
        ),
        ("target", "(define-public (run) (ok u1))"),
        (
            "caller",
            "(use-trait t .definition.t) (define-public (run (target <t>)) (begin (contract-of target) (contract-of target) (ok (contract-of target))))",
        ),
    ] {
        env.initialize_versioned_contract(
            QualifiedContractIdentifier::local(name).unwrap(),
            ClarityVersion::Clarity2,
            source,
            None,
        )
        .unwrap();
    }
    drop(env);
    let target = QualifiedContractIdentifier::local("target").unwrap();
    let caller = QualifiedContractIdentifier::local("caller").unwrap();
    for pending in [true, false] {
        if !pending {
            store
                .get_side_store()
                .execute(
                    "UPDATE metadata_table SET value=?1 WHERE key=?2",
                    rusqlite::params![
                        vec![255u8; 1024 * 1024],
                        format!("clr-meta::{target}::{CONTRACT_HEADER_KEY}")
                    ],
                )
                .unwrap();
        }
        let mut db = store.as_clarity_db();
        db.begin();
        if pending {
            db.store
                .insert_metadata_value(
                    &target,
                    CONTRACT_HEADER_KEY,
                    MetadataValue::Blob(vec![255u8; 1024 * 1024].into()),
                )
                .unwrap();
        }
        assert!(db.has_contract(&target).unwrap());
        assert!(
            !db.has_contract(&QualifiedContractIdentifier::local("absent").unwrap())
                .unwrap()
        );
        let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
        let result = env
            .execute_transaction(
                caller.issuer.clone().into(),
                None,
                caller.clone(),
                "run",
                &[SymbolicExpression::atom_value(Value::from(target.clone()))],
            )
            .unwrap();
        assert_eq!(result.0, Value::okay(Value::from(target.clone())).unwrap());
        let (mut db, _) = env.destruct().unwrap();
        db.roll_back().unwrap();
    }
    // A missing principal reaches contract-of through argument binding too.
    let absent = QualifiedContractIdentifier::local("absent").unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, store.as_clarity_db(), epoch);
    let error = env
        .execute_transaction(
            caller.issuer.clone().into(),
            None,
            caller,
            "run",
            &[SymbolicExpression::atom_value(Value::from(absent.clone()))],
        )
        .unwrap_err();
    assert_eq!(
        error,
        VmExecutionError::from(RuntimeCheckErrorKind::NoSuchContract(absent.to_string()))
    );
    drop(env);
    store
        .get_side_store()
        .execute("DROP TABLE metadata_table", [])
        .unwrap();
    let mut db = store.as_clarity_db();
    db.begin();
    assert!(
        db.has_contract(&target).is_err(),
        "storage errors are not absence"
    );
}

/// Simulate an incomplete dependency scan without deleting the declaration.
#[test]
fn omitted_dependency_is_an_internal_error() {
    use super::super::clarity_store::ClarityBackingStore;
    let epoch = StacksEpochId::Epoch41;
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, epoch);
    let key = format!("clr-meta::{id}::{CONTRACT_HEADER_KEY}");
    let conn = store.get_side_store();
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT value FROM metadata_table WHERE key=?1",
            [&key],
            |r| r.get(0),
        )
        .unwrap();
    let mut header = super::super::contract_codec::decode_header(&bytes).unwrap();
    for entry in &mut header.functions {
        entry.dependencies.clear();
    }
    conn.execute(
        "UPDATE metadata_table SET value=?1 WHERE key=?2",
        rusqlite::params![
            super::super::contract_codec::encode_header(&header).unwrap(),
            key
        ],
    )
    .unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, store.as_clarity_db(), epoch);
    let error = env
        .execute_transaction(id.issuer.clone().into(), None, id, "run", &[])
        .unwrap_err();
    assert!(
        matches!(error, VmExecutionError::Internal(VmInternalError::Expect(ref message)) if message.contains("omitted function leaf"))
    );
    assert_eq!(env.context.cost_track.get_memory(), 0);
}

#[test]
fn trait_preflight_loads_only_the_definition_needed_for_implicit_checks() {
    use crate::vm::costs::{ExecutionCost, LimitedCostTracker};
    for epoch in [
        StacksEpochId::Epoch21,
        StacksEpochId::Epoch22,
        StacksEpochId::Epoch40,
        StacksEpochId::Epoch41,
    ] {
        let mut store = MemoryBackingStore::new();
        let mut db = store.as_clarity_db();
        db.begin();
        db.set_clarity_epoch_version(epoch).unwrap();
        db.commit().unwrap();
        let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
        for (name, source) in [
            (
                "definition",
                "(define-trait t ((run () (response uint uint))))",
            ),
            (
                "explicit",
                "(impl-trait .definition.t) (define-private (leaf) u42) (define-public (run) (ok (leaf)))",
            ),
            (
                "implicit",
                "(define-private (leaf) u42) (define-public (run) (ok (leaf)))",
            ),
            (
                "caller",
                "(use-trait t .definition.t) (define-public (call (target <t>)) (contract-call? target run))",
            ),
        ] {
            env.initialize_versioned_contract(
                QualifiedContractIdentifier::local(name).unwrap(),
                ClarityVersion::Clarity2,
                source,
                None,
            )
            .unwrap();
        }
        drop(env);
        let caller = QualifiedContractIdentifier::local("caller").unwrap();
        let trait_id = TraitIdentifier {
            name: ClarityName::from_literal("t"),
            contract_identifier: QualifiedContractIdentifier::local("definition").unwrap(),
        };
        for (name, definition_count) in [("explicit", 0), ("implicit", 1)] {
            let id = QualifiedContractIdentifier::local(name).unwrap();
            let mut expected = None;
            for cached in [false, true] {
                let mut cache = ClarityExecutionCache::default();
                let db = store.as_clarity_db();
                let db = if cached {
                    db.with_cache(&mut cache)
                } else {
                    db
                };
                let mut env = OwnedEnvironment::new_cost_limited(
                    false,
                    CHAIN_ID_TESTNET,
                    db,
                    LimitedCostTracker::new_with_limit(epoch, ExecutionCost::max_value()),
                    epoch,
                );
                let result = env
                    .execute_transaction(
                        caller.issuer.clone().into(),
                        None,
                        caller.clone(),
                        "call",
                        &[SymbolicExpression::atom_value(Value::Principal(
                            id.clone().into(),
                        ))],
                    )
                    .unwrap();
                assert_eq!(result.0, Value::okay(Value::UInt(42)).unwrap());
                let observation = (result, env.get_cost_total());
                if let Some(expected) = &expected {
                    assert_eq!(&observation, expected);
                } else {
                    expected = Some(observation);
                }
            }
            let mut db = store.as_clarity_db();
            db.begin();
            db.store
                .insert_metadata_value(
                    &id,
                    &function_key("leaf"),
                    MetadataValue::Blob(b"invalid dependency".as_slice().into()),
                )
                .unwrap();
            let preflight = db
                .get_contract_for_trait_check(&id, "run", &trait_id)
                .unwrap();
            match preflight {
                super::super::clarity_db::TraitCheck::Explicit => {
                    assert_eq!(definition_count, 0)
                }
                super::super::clarity_db::TraitCheck::NeedsSignatureCheck(function) => {
                    assert_eq!(usize::from(function.is_some()), definition_count);
                }
            }
            let missing = db
                .get_contract_for_trait_check(&id, "missing", &trait_id)
                .unwrap();
            if let super::super::clarity_db::TraitCheck::NeedsSignatureCheck(function) = missing {
                assert!(function.is_none());
            }
            // Dependencies are decoded only by the charged execution load.
            let mut charged = false;
            let result = db.load_contract_for_call(&id, "run", |_| {
                charged = true;
                Ok(())
            });
            assert!(charged);
            assert!(result.is_err());
            db.roll_back().unwrap();
        }
    }
}

#[test]
fn directory_rejects_invalid_indices_and_names_and_handles_cycles() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    let mut db = store.as_clarity_db();
    db.begin();
    let full = db.get_contract(&id).unwrap();
    let (header, _) = split_contract(&full).unwrap();
    let mut invalid = header.clone();
    invalid.functions[0].dependencies.push(u32::MAX);
    assert!(invalid.validate_directory().is_err());
    let mut invalid = header.clone();
    invalid.functions[1].name = invalid.functions[0].name.clone();
    assert!(invalid.validate_directory().is_err());
    let mut invalid = header.clone();
    invalid.functions.swap(0, 1);
    assert!(invalid.validate_directory().is_err());
    let mut header = header;
    // Cycles cannot be deployed by analysis, but a corrupt cycle must still
    // terminate and select each record once rather than recurse forever.
    header.functions[0].dependencies = vec![1, 1];
    header.functions[1].dependencies = vec![0];
    header.validate_directory().unwrap();
    let names: Vec<_> = header.functions[..2]
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    let loaded = LoadedContractHeader {
        shared: Arc::new({
            let mut shared = Arc::unwrap_or_clone(header.shared);
            shared.function_names = header.functions.iter().map(|e| e.name.clone()).collect();
            shared
        }),
        dependencies: header
            .functions
            .into_iter()
            .map(|e| e.dependencies)
            .collect(),
        deployment: None,
        load_cost_size: 0,
    };
    assert_eq!(
        loaded.dependency_closure(&names[0]).unwrap(),
        names.iter().collect::<Vec<_>>()
    );
    let expression = SymbolicExpression::list(
        names
            .iter()
            .cloned()
            .map(SymbolicExpression::atom)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        loaded.selection_for_expression(&expression).unwrap(),
        names.iter().collect::<Vec<_>>()
    );
    db.roll_back().unwrap();
}

#[test]
fn default_cost_tracker_requires_metadata_but_does_not_read_bodies() {
    use crate::boot_util::boot_code_id;
    use crate::vm::costs::{CostErrors, ExecutionCost, LimitedCostTracker};
    use crate::vm::database::ClarityBackingStore;

    let epoch = StacksEpochId::Epoch33;
    let id = boot_code_id("costs-4", false);
    let mut store = MemoryBackingStore::new();
    let mut db = store.as_clarity_db();
    db.begin();
    db.set_clarity_epoch_version(epoch).unwrap();
    db.commit().unwrap();
    assert!(matches!(
        LimitedCostTracker::new(
            false,
            CHAIN_ID_TESTNET,
            ExecutionCost::max_value(),
            &mut db,
            epoch
        ),
        Err(CostErrors::CostContractLoadFailure)
    ));
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
    env.initialize_versioned_contract(
        id.clone(),
        ClarityVersion::Clarity2,
        "(define-read-only (unused) u1)",
        None,
    )
    .unwrap();
    drop(env);
    // A body read would fail to decode this deliberately corrupt record.
    store
        .get_side_store()
        .execute(
            "UPDATE metadata_table SET value=X'00' WHERE key=?1",
            [format!("clr-meta::{id}::{}", function_key("unused"))],
        )
        .unwrap();
    let mut db = store.as_clarity_db();
    LimitedCostTracker::new(
        false,
        CHAIN_ID_TESTNET,
        ExecutionCost::max_value(),
        &mut db,
        epoch,
    )
    .unwrap();
    db.begin();
    assert!(db.get_contract(&id).is_err());
    db.roll_back().unwrap();
}

#[test]
fn charged_calls_resolve_once_and_cache_hits_resolve_zero_times() {
    for epoch in [StacksEpochId::Epoch40, StacksEpochId::Epoch41] {
        for cached in [false, true] {
            let mut store = MemoryBackingStore::new();
            let id = deploy(&mut store, epoch);
            let mut cache = ClarityExecutionCache::default();
            for pass in 0..2 {
                let before = store.metadata_resolutions;
                let batches_before = store.metadata_batches;
                let mut db = store.as_clarity_db();
                if cached {
                    db = db.with_cache(&mut cache);
                }
                db.begin();
                let mut size = 0;
                let loaded = db.load_contract_for_call(&id, "run", |amount| {
                    size = amount;
                    Ok(())
                });
                let contract = loaded.unwrap();
                assert_eq!(contract.functions.len(), 2);
                assert!(contract.lookup_function("unused").is_none());
                // Verify the unchanged historical charge using the
                // independent public accounting path after counting resolutions.
                db.roll_back().unwrap();
                drop(db);
                assert_eq!(
                    store.metadata_resolutions - before,
                    if cached && pass > 0 { 0 } else { 1 },
                    "{epoch:?}, cached={cached}"
                );
                assert_eq!(
                    store.metadata_batches - batches_before,
                    if cached && pass > 0 { 0 } else { 2 }
                );
                let mut db = store.as_clarity_db();
                db.begin();
                assert_eq!(size, db.get_contract_size(&id).unwrap());
                db.roll_back().unwrap();
            }
        }
    }
}

#[test]
fn charging_preserves_header_error_order_and_defers_body_errors() {
    for epoch in [StacksEpochId::Epoch40, StacksEpochId::Epoch41] {
        let mut store = MemoryBackingStore::new();
        let id = deploy(&mut store, epoch);
        let mut db = store.as_clarity_db();
        db.begin();
        db.store
            .insert_metadata_value(
                &id,
                CONTRACT_HEADER_KEY,
                MetadataValue::Blob(b"invalid header".as_slice().into()),
            )
            .unwrap();
        let mut size = 0;
        let loaded = db.load_contract_for_call(&id, "run", |amount| {
            size = amount;
            Ok(())
        });
        assert_eq!(size, db.get_contract_size(&id).unwrap());
        assert!(loaded.is_err());
        let charged_error = db
            .load_contract_for_call(&id, "run", |_| {
                Err(VmInternalError::Expect("charge rejected".into()).into())
            })
            .err()
            .unwrap();
        assert_eq!(
            charged_error,
            VmInternalError::Expect("charge rejected".into()).into()
        );
        // A successful reservation is released even if decoding then fails.
        let mut env = OwnedEnvironment::new_cost_limited(
            false,
            CHAIN_ID_TESTNET,
            db,
            crate::vm::costs::LimitedCostTracker::new_with_limit(
                epoch,
                crate::vm::costs::ExecutionCost::max_value(),
            ),
            epoch,
        );
        assert!(
            env.execute_transaction(id.issuer.clone().into(), None, id.clone(), "run", &[])
                .is_err()
        );
        let (mut db, tracker) = env.destruct().unwrap();
        assert_eq!(tracker.get_memory(), 0);
        db.roll_back().unwrap();
        db.begin();
        db.store
            .insert_metadata_value(
                &id,
                &function_key("leaf"),
                MetadataValue::Blob(b"invalid body".as_slice().into()),
            )
            .unwrap();
        let loaded = db.load_contract_for_call(&id, "run", |amount| {
            let _ = amount;
            Ok(())
        });
        assert!(loaded.is_err());
        // Unselected malformed code does not change entrypoint semantics.
        let loaded = db.load_contract_for_call(&id, "unused", |amount| {
            let _ = amount;
            Ok(())
        });
        assert!(loaded.is_ok());
        db.roll_back().unwrap();
    }
}

#[test]
fn missing_entry_preserves_charge_and_complete_namespace() {
    for epoch in [StacksEpochId::Epoch40, StacksEpochId::Epoch41] {
        let mut store = MemoryBackingStore::new();
        let id = deploy(&mut store, epoch);
        let mut db = store.as_clarity_db();
        db.begin();
        let expected = db.get_contract_size(&id).unwrap();
        let mut size = 0;
        let loaded = db.load_contract_for_call(&id, "missing", |amount| {
            size = amount;
            Ok(())
        });
        assert_eq!(size, expected);
        let contract = loaded.unwrap();
        assert!(contract.functions.is_empty());
        assert!(contract.has_function("run"));
        db.roll_back().unwrap();
    }
}

#[test]
fn loads_only_transitive_bodies_and_preserves_the_namespace() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch40);
    let mut db = store.as_clarity_db();
    db.begin();
    for (entry, expected) in [
        ("run", vec!["leaf", "run"]),
        ("branches", vec!["branches", "leaf", "other"]),
        ("mapped", vec!["leaf", "mapped"]),
        ("tuple-key", vec!["leaf", "tuple-key", "unused"]),
        ("bad-binding", vec!["bad-binding", "unused"]),
    ] {
        let contract = db.get_contract_for_function(&id, entry).unwrap();
        let names: BTreeSet<_> = contract.functions.keys().map(|s| s.as_str()).collect();
        assert_eq!(names, expected.into_iter().collect());
        assert!(contract.has_function("unused"));
        assert_eq!(contract.lookup_variable("answer"), Some(&Value::UInt(42)));
    }
    let full = db.get_contract(&id).unwrap();
    assert_eq!(full.functions.len(), 8);
    db.roll_back().unwrap();
}

#[test]
fn cached_parts_share_constants_and_function_bodies() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch40);
    let mut cache = ClarityExecutionCache::default();
    let mut db = store.as_clarity_db().with_cache(&mut cache);
    db.begin();
    let first = db.get_contract_for_function(&id, "run").unwrap();
    let second = db.get_contract_for_function(&id, "branches").unwrap();
    assert!(std::ptr::eq(&*first.shared, &*second.shared));
    assert!(std::ptr::eq(
        first.lookup_function("leaf").unwrap().body(),
        second.lookup_function("leaf").unwrap().body()
    ));
    assert!(std::ptr::eq(
        first.lookup_function("run").unwrap(),
        first.functions.get("run").unwrap()
    ));
    let again = db.get_contract_for_function(&id, "run").unwrap();
    assert!(std::ptr::eq(&*first.shared, &*again.shared));
    assert!(std::ptr::eq(
        first.lookup_function("run").unwrap().body(),
        again.lookup_function("run").unwrap().body()
    ));
    db.roll_back().unwrap();
}

#[test]
fn pending_function_overrides_bypass_cached_parts_and_rollback_restores_them() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    let mut cache = ClarityExecutionCache::default();
    let mut db = store.as_clarity_db().with_cache(&mut cache);
    db.begin();
    let original = db.get_contract_for_function(&id, "run").unwrap();
    let mut record = FunctionDefinition::from(original.lookup_function("leaf").unwrap());
    record.body = SymbolicExpression::atom_value(Value::UInt(u128::MAX));
    db.begin();
    db.store
        .insert_metadata_value(
            &id,
            &function_key("leaf"),
            MetadataValue::Blob(
                super::super::contract_codec::encode_function(&record)
                    .unwrap()
                    .into(),
            ),
        )
        .unwrap();
    let overridden = db.get_contract_for_function(&id, "run").unwrap();
    assert_eq!(
        overridden
            .lookup_function("leaf")
            .unwrap()
            .body()
            .match_atom_value(),
        Some(&Value::UInt(u128::MAX))
    );
    db.roll_back().unwrap();
    let restored = db.get_contract_for_function(&id, "run").unwrap();
    assert!(std::ptr::eq(&*original.shared, &*restored.shared));
    assert!(std::ptr::eq(
        original.lookup_function("leaf").unwrap().body(),
        restored.lookup_function("leaf").unwrap().body()
    ));
    db.roll_back().unwrap();
}

#[test]
fn binary_records_follow_nested_commit_and_rollback() {
    fn insert(
        db: &mut crate::vm::database::ClarityDatabase,
        name: &str,
    ) -> QualifiedContractIdentifier {
        let id = QualifiedContractIdentifier::local(name).unwrap();
        let mut context = ContractContext::new(id.clone(), ClarityVersion::Clarity2);
        let name = ClarityName::from_literal("value");
        context.functions.insert(
            name.clone(),
            DefinedFunction::new(
                vec![],
                SymbolicExpression::atom_value(Value::UInt(42)),
                DefineType::ReadOnly,
                &name,
                &id.to_string(),
            ),
        );
        db.insert_contract_hash(&id, "source").unwrap();
        db.set_contract_data_size(&id, 0).unwrap();
        db.insert_contract(&id, context.into()).unwrap();
        id
    }
    let mut store = MemoryBackingStore::new();
    let mut db = store.as_clarity_db();
    db.begin();
    db.set_clarity_epoch_version(StacksEpochId::Epoch41)
        .unwrap();
    let outer = insert(&mut db, "outer");
    db.begin();
    let child = insert(&mut db, "child");
    db.begin();
    let grandchild = insert(&mut db, "grandchild");
    db.commit().unwrap();
    assert!(db.get_contract(&grandchild).is_ok());
    db.roll_back().unwrap();
    assert!(db.get_contract(&child).is_err());
    assert!(db.get_contract(&grandchild).is_err());
    db.begin();
    let committed = insert(&mut db, "committed");
    db.commit().unwrap();
    db.commit().unwrap();
    drop(db);
    // A fresh view must read the committed BLOBs, not a pending overlay/cache.
    let mut db = store.as_clarity_db();
    db.begin();
    for id in [&outer, &committed] {
        let values = db
            .store
            .get_metadata_batch(
                id,
                &[CONTRACT_HEADER_KEY.into(), function_key("value")],
                &mut None,
            )
            .unwrap();
        assert!(
            values
                .iter()
                .all(|value| matches!(value, Some(MetadataValue::Blob(_))))
        );
        let context = db.get_contract(id).unwrap();
        assert_eq!(
            context
                .lookup_function("value")
                .unwrap()
                .body()
                .match_atom_value(),
            Some(&Value::UInt(42))
        );
    }
    assert!(db.get_contract(&child).is_err());
    assert!(db.get_contract(&grandchild).is_err());
    db.roll_back().unwrap();
}

/// Every record fits, but they cannot all coexist: nested calls must evict and
/// reload during one transaction without changing results, costs, or memory.
#[test]
fn nested_calls_execute_with_real_part_eviction() {
    use crate::vm::costs::{ExecutionCost, LimitedCostTracker};
    use crate::vm::database::{ClarityBackingStore, ContractCache};
    let epoch = StacksEpochId::Epoch41;
    let mut store = MemoryBackingStore::new();
    let mut db = store.as_clarity_db();
    db.begin();
    db.set_clarity_epoch_version(epoch).unwrap();
    db.commit().unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
    for (name, source) in [
        (
            "first",
            "(define-private (leaf) u42) (define-public (run) (ok (leaf)))",
        ),
        (
            "second",
            "(define-private (leaf) u42) (define-public (run) (ok (leaf)))",
        ),
        (
            "driver",
            "(define-public (run) (begin (contract-call? .first run) (contract-call? .second run) (contract-call? .first run) (contract-call? .second run)))",
        ),
    ] {
        env.initialize_versioned_contract(
            QualifiedContractIdentifier::local(name).unwrap(),
            ClarityVersion::Clarity2,
            source,
            None,
        )
        .unwrap();
    }
    drop(env);
    let largest: u64 = store
        .get_side_store()
        .query_row(
            "SELECT max(length(value)) FROM metadata_table WHERE typeof(value)='blob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let id = QualifiedContractIdentifier::local("driver").unwrap();
    let mut expected = None;
    for budget in [5 * 1024 * 1024, largest] {
        let mut cache = ClarityExecutionCache {
            contracts: ContractCache::new(budget),
        };
        let mut env = OwnedEnvironment::new_cost_limited(
            false,
            CHAIN_ID_TESTNET,
            store.as_clarity_db().with_cache(&mut cache),
            LimitedCostTracker::new_with_limit(epoch, ExecutionCost::max_value()),
            epoch,
        );
        let result = env
            .execute_transaction(id.issuer.clone().into(), None, id.clone(), "run", &[])
            .unwrap();
        let actual = (
            result,
            env.get_cost_total(),
            env.context.cost_track.get_memory(),
        );
        drop(env);
        if let Some(expected) = &expected {
            assert_eq!(&actual, expected);
        } else {
            expected = Some(actual);
        }
        if budget == largest {
            assert!(!cache.contracts.is_empty(), "parts fit and are retained");
            // Eight distinct parts: two for driver, three for each callee.
            // More misses proves that retained parts were evicted and loaded again.
            assert!(cache.contracts.misses() > 8);
            assert!(cache.contracts.total_weight() <= budget);
        }
    }
}

#[test]
fn eviction_preserves_results_and_historical_charges() {
    use crate::vm::database::caching::ContractCache;
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    let mut db = store.as_clarity_db();
    db.begin();
    let expected_size = db.get_contract_size(&id).unwrap();
    db.roll_back().unwrap();
    drop(db);
    for limit in [1, expected_size, expected_size * 2] {
        let mut cache = ClarityExecutionCache {
            contracts: ContractCache::new(limit),
        };
        for entry in ["run", "branches", "mapped", "run"] {
            let mut db = store.as_clarity_db().with_cache(&mut cache);
            db.begin();
            let mut size = 0;
            let loaded = db.load_contract_for_call(&id, entry, |amount| {
                size = amount;
                Ok(())
            });
            let bundle = loaded.unwrap();
            assert!(bundle.lookup_function(entry).is_some());
            assert!(bundle.lookup_function("leaf").is_some());
            assert!(bundle.lookup_function("unused").is_none());
            assert_eq!(size, expected_size);
            db.roll_back().unwrap();
            drop(db);
            assert!(cache.contracts.total_weight() <= limit);
        }
    }
}

#[test]
fn selected_execution_and_unloaded_name_collisions() {
    for epoch in [StacksEpochId::Epoch40, StacksEpochId::Epoch41] {
        let mut store = MemoryBackingStore::new();
        let id = deploy(&mut store, epoch);
        let mut env =
            OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, store.as_clarity_db(), epoch);
        for entry in ["run", "branches", "tuple-key"] {
            let result = env
                .execute_transaction(id.issuer.clone().into(), None, id.clone(), entry, &[])
                .unwrap();
            assert_eq!(result.0, Value::okay(Value::UInt(42)).unwrap());
        }
        let error = env
            .execute_transaction(
                id.issuer.clone().into(),
                None,
                id.clone(),
                "bad-binding",
                &[],
            )
            .unwrap_err();
        assert_eq!(
            error,
            RuntimeCheckErrorKind::NameAlreadyUsed("unused".into()).into()
        );
    }
}

#[test]
fn exact_fit_cache_counts_each_part_once_and_retains_deployment_binding() {
    use crate::vm::database::caching::ContractCache;
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    let mut cache = ClarityExecutionCache::default();
    {
        let mut db = store.as_clarity_db().with_cache(&mut cache);
        db.begin();
        db.get_contract_for_function(&id, "run").unwrap();
        db.roll_back().unwrap();
    }
    let budget = cache.contracts.total_weight();
    cache.contracts = ContractCache::new(budget);
    let original = {
        let mut db = store.as_clarity_db().with_cache(&mut cache);
        db.begin();
        let mut size = 0;
        let loaded = db.load_contract_for_call(&id, "run", |amount| {
            size = amount;
            Ok(())
        });
        assert_eq!(size, db.get_contract_size(&id).unwrap());
        let contract = loaded.unwrap();
        db.roll_back().unwrap();
        contract
    };
    assert_eq!(
        cache.contracts.len(),
        3,
        "one header and two functions, no duplicate bundle"
    );
    assert_eq!(cache.contracts.total_weight(), budget);
    let before = (
        store.metadata_resolutions,
        store.block_height_reads,
        store.data_reads,
    );
    let mut db = store.as_clarity_db().with_cache(&mut cache);
    db.begin();
    let loaded = db.load_contract_for_call(&id, "run", |amount| {
        let _ = amount;
        Ok(())
    });
    let contract = loaded.unwrap();
    assert!(std::ptr::eq(&*original.shared, &*contract.shared));
    assert!(std::ptr::eq(
        original.lookup_function("run").unwrap().body(),
        contract.lookup_function("run").unwrap().body()
    ));
    db.roll_back().unwrap();
    drop(db);
    assert_eq!(store.metadata_resolutions - before.0, 0);
    assert_eq!(store.block_height_reads - before.1, 0);
    assert_eq!(
        store.data_reads - before.2,
        1,
        "the stored epoch read remains outside this change"
    );
}

#[test]
fn historical_charge_survives_epoch_transition_and_cache_hits() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch40);
    let mut cache = ClarityExecutionCache::default();
    let expected_size;
    {
        let mut db = store.as_clarity_db().with_cache(&mut cache);
        db.begin();
        expected_size = db.get_contract_size(&id).unwrap();
        assert!(expected_size > SOURCE.len() as u64); // Includes constant data.
        for epoch in [StacksEpochId::Epoch40, StacksEpochId::Epoch41] {
            db.set_clarity_epoch_version(epoch).unwrap();
            // Commit the epoch before loading so the cache is eligible.
            db.commit().unwrap();
            db.begin();
            for entry in ["run", "run", "branches", "missing"] {
                let mut size = 0;
                let loaded = db.load_contract_for_call(&id, entry, |amount| {
                    size = amount;
                    Ok(())
                });
                assert_eq!(size, expected_size, "{epoch:?}, {entry}");
                loaded.unwrap();
            }
        }
        db.commit().unwrap();
    }
    let mut db = store.as_clarity_db();
    db.begin();
    let mut size = 0;
    let loaded = db.load_contract_for_call(&id, "run", |amount| {
        size = amount;
        Ok(())
    });
    assert_eq!(size, expected_size);
    loaded.unwrap();
    db.roll_back().unwrap();
}

#[test]
fn split_roundtrip_matches_legacy_independently_of_function_order() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch40);
    let mut db = store.as_clarity_db();
    db.begin();
    let full = db.get_contract(&id).unwrap();
    let serialized = ClaritySerializable::serialize(&*full);
    let legacy = ContractContext::deserialize(&serialized).unwrap();
    let (header, records) = split_contract(&legacy).unwrap();
    for (name, record) in &records {
        assert!(std::ptr::eq(
            record.body(),
            legacy.lookup_function(name).unwrap().body()
        ));
    }
    let mut reordered = legacy.clone();
    let entries: Vec<_> = legacy.functions.iter().collect();
    reordered.functions = entries
        .into_iter()
        .rev()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let (other, _) = split_contract(&reordered).unwrap();
    assert_eq!(
        ClaritySerializable::serialize(&header),
        ClaritySerializable::serialize(&other)
    );
    let rebuilt = ContractContext {
        shared: Arc::new({
            let mut shared = header.shared.as_ref().clone();
            shared.function_names = header.functions.iter().map(|e| e.name.clone()).collect();
            shared
        }),
        functions: records.into_iter().collect(),
    };
    assert_eq!(
        serde_json::to_value(&legacy).unwrap(),
        serde_json::to_value(&rebuilt).unwrap()
    );
    db.roll_back().unwrap();
}

#[test]
fn serialized_contract_resolves_once_and_preserves_all_functions() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    let before = store.metadata_resolutions;
    let mut db = store.as_clarity_db();
    db.begin();
    let json = db.get_serialized_contract(&id).unwrap().unwrap();
    let context = ContractContext::deserialize(&json).unwrap();
    assert_eq!(context.functions.len(), 8);
    assert_eq!(context.variables.get("answer"), Some(&Value::UInt(42)));
    db.roll_back().unwrap();
    drop(db);
    assert_eq!(store.metadata_resolutions - before, 1);
}

#[test]
fn read_only_expressions_load_only_their_local_dependencies() {
    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, StacksEpochId::Epoch41);
    let mut env = OwnedEnvironment::new_free(
        false,
        CHAIN_ID_TESTNET,
        store.as_clarity_db(),
        StacksEpochId::Epoch41,
    );
    for (program, expected) in [
        ("(run)", Value::okay(Value::UInt(42)).unwrap()),
        ("(get unused (tuple (unused (leaf u0))))", Value::UInt(42)),
        ("(if true (leaf u0) (other))", Value::UInt(42)),
        ("answer", Value::UInt(42)),
    ] {
        assert_eq!(env.eval_read_only(&id, program).unwrap().0, expected);
    }
    drop(env);
    let mut db = store.as_clarity_db();
    db.begin();
    let expression = SymbolicExpression::list(vec![SymbolicExpression::atom(
        ClarityName::from_literal("run"),
    )]);
    let context = db.get_contract_for_expression(&id, &expression).unwrap();
    assert_eq!(context.functions.len(), 2);
    assert!(!context.functions.contains_key("unused"));
    db.roll_back().unwrap();
}

#[test]
fn sanitizer_certificate_includes_type_metadata_and_preserves_failures() {
    use crate::vm::types::{ListTypeData, SequenceData};
    let id = QualifiedContractIdentifier::local("constants").unwrap();
    for valid in [true, false] {
        let mut value = Value::cons_list_unsanitized(vec![if valid {
            Value::UInt(1)
        } else {
            Value::Bool(true)
        }])
        .unwrap();
        let Value::Sequence(SequenceData::List(list)) = &mut value else {
            unreachable!()
        };
        list.type_signature = ListTypeData::new_list(TypeSignature::UIntType, 4).unwrap();
        if valid {
            let (normalized, changed) = Value::sanitize_value(
                &StacksEpochId::Epoch34,
                &TypeSignature::type_of(&value).unwrap(),
                value.clone(),
            )
            .unwrap();
            assert!(
                !changed,
                "the sanitizer flag does not capture the shorter list type"
            );
            assert_eq!(
                value, normalized,
                "Value equality ignores list type metadata too"
            );
            assert_ne!(
                postcard::to_allocvec(&value).unwrap(),
                postcard::to_allocvec(&normalized).unwrap()
            );
        }
        let mut context = ContractContext::new(id.clone(), ClarityVersion::Clarity2);
        context
            .shared_mut()
            .variables
            .insert(ClarityName::from_literal("constant"), value);
        let (header, _) = split_contract(&context).unwrap();
        assert!(!header.constants_sanitized);
        for epoch in [
            StacksEpochId::Epoch33,
            StacksEpochId::Epoch34,
            StacksEpochId::Epoch40,
            StacksEpochId::Epoch41,
        ] {
            let mut store = MemoryBackingStore::new();
            let mut db = store.as_clarity_db();
            db.begin();
            db.set_clarity_epoch_version(epoch).unwrap();
            db.insert_contract_hash(&id, "source").unwrap();
            db.set_contract_data_size(&id, 0).unwrap();
            db.insert_contract(&id, context.clone().into()).unwrap();
            db.commit().unwrap();
            db.begin();
            let actual = db.get_contract_metadata(&id);
            let mut expected = context.clone();
            let result = expected.canonicalize_types(&epoch);
            match (actual, result) {
                (Ok(actual), Ok(())) => assert_eq!(
                    serde_json::to_value(&*actual).unwrap(),
                    serde_json::to_value(&expected.shared).unwrap()
                ),
                (Err(actual), Err(expected)) => assert_eq!(actual, expected),
                _ => panic!("canonicalization behavior changed for {epoch:?}, valid={valid}"),
            }
            db.roll_back().unwrap();
        }
    }
}

#[test]
fn metered_execution_matches_eager_execution_and_is_independent_of_cache() {
    use crate::vm::costs::cost_functions::ClarityCostFunction;
    use crate::vm::costs::{CostTracker, ExecutionCost, LimitedCostTracker, runtime_cost};
    for epoch in [
        StacksEpochId::Epoch20,
        StacksEpochId::Epoch2_05,
        StacksEpochId::Epoch21,
        StacksEpochId::Epoch33,
        StacksEpochId::Epoch34,
        StacksEpochId::Epoch40,
        StacksEpochId::Epoch41,
    ] {
        let mut store = MemoryBackingStore::new();
        let id = deploy(&mut store, epoch);
        let mut db = store.as_clarity_db();
        db.begin();
        let raw = db.get_serialized_contract(&id).unwrap().unwrap();
        let load_cost_size = db.get_contract_size(&id).unwrap();
        db.roll_back().unwrap();
        let mut eager = ContractContext::deserialize(&raw).unwrap();
        eager.canonicalize_types(&epoch).unwrap();
        let mut baseline = OwnedEnvironment::new_cost_limited(
            false,
            CHAIN_ID_TESTNET,
            db,
            LimitedCostTracker::new_with_limit(epoch, ExecutionCost::max_value()),
            epoch,
        );
        let expected = baseline
            .execute_in_env(
                id.issuer.clone().into(),
                None,
                Some(eager),
                |state, invocation| -> Result<_, VmExecutionError> {
                    runtime_cost(ClarityCostFunction::LoadContract, state, load_cost_size)?;
                    state.add_memory(load_cost_size)?;
                    let result = invocation
                        .contract_context
                        .lookup_function("run")
                        .unwrap()
                        .apply(&[], state, invocation);
                    state.drop_memory(load_cost_size)?;
                    result
                },
            )
            .unwrap();
        let expected_cost = baseline.get_cost_total();
        drop(baseline);
        let mut cache = ClarityExecutionCache::default();
        for cached in [false, true, true] {
            let db = if cached {
                store.as_clarity_db().with_cache(&mut cache)
            } else {
                store.as_clarity_db()
            };
            let mut env = OwnedEnvironment::new_cost_limited(
                false,
                CHAIN_ID_TESTNET,
                db,
                LimitedCostTracker::new_with_limit(epoch, ExecutionCost::max_value()),
                epoch,
            );
            let actual = env
                .execute_transaction(id.issuer.clone().into(), None, id.clone(), "run", &[])
                .unwrap();
            assert_eq!(actual, expected, "{epoch:?}");
            assert_eq!(env.get_cost_total(), expected_cost, "{epoch:?}");
            assert_eq!(env.context.cost_track.get_memory(), 0);
        }
    }
}
