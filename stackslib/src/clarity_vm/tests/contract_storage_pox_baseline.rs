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

//! Real MARF/PoX baseline from unchanged 3fd5d990. Copy this module and its JSON
//! fixture into that checkout and register it in tests/mod.rs. Setting
//! CONTRACT_STORAGE_POX_OUTPUT writes observations before comparing the fixture.
//! Commit roots cover all map/variable writes and native STX-lock state.
use clarity::vm::contexts::OwnedEnvironment;
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::test_util::{generate_test_burn_state_db, symbols_from_values, TEST_HEADER_DB};
use clarity::vm::types::{PrincipalData, StandardPrincipalData, TupleData};
use clarity::vm::{ClarityName, ClarityVersion, Value};
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::chainstate::{StacksBlockId, StacksPrivateKey, StacksPublicKey};
use stacks_common::types::StacksEpochId;

use crate::chainstate::stacks::boot::{POX_2_TESTNET_CODE, POX_3_TESTNET_CODE, POX_4_CODE};
use crate::chainstate::stacks::index::ClarityMarfTrieId;
use crate::clarity_vm::clarity::{ClarityMarfStore, ClarityMarfStoreTransaction};
use crate::clarity_vm::database::marf::MarfedKV;
use crate::core::{FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH};
use crate::util_lib::boot::boot_code_id;

#[test]
fn pox_events_and_committed_state_match_original_json_loader() {
    let mut observations = std::collections::BTreeMap::new();
    for (name, source, epoch) in [
        ("pox-2", POX_2_TESTNET_CODE.as_str(), StacksEpochId::Epoch21),
        ("pox-3", POX_3_TESTNET_CODE.as_str(), StacksEpochId::Epoch24),
        ("pox-4", POX_4_CODE.as_str(), StacksEpochId::Epoch25),
        ("pox-4", POX_4_CODE.as_str(), StacksEpochId::Epoch30),
        ("pox-4", POX_4_CODE.as_str(), StacksEpochId::Epoch34),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut marf = MarfedKV::open(dir.path().to_str().unwrap(), None, None).unwrap();
        let initial = StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
        let burn = generate_test_burn_state_db(epoch);
        let id = boot_code_id(name, false);
        let principals: Vec<PrincipalData> = (1u8..=3)
            .map(|byte| {
                let mut key = StacksPrivateKey::from_slice(&[byte; 32]).unwrap();
                key.set_compress_public(true);
                StandardPrincipalData::from(&key).into()
            })
            .collect();
        let pox_addr = Value::Tuple(
            TupleData::from_data(vec![
                (
                    ClarityName::from_literal("version"),
                    Value::buff_from(vec![0]).unwrap(),
                ),
                (
                    ClarityName::from_literal("hashbytes"),
                    Value::buff_from(vec![1; 20]).unwrap(),
                ),
            ])
            .unwrap(),
        );
        {
            let mut store = marf.begin(&StacksBlockId::sentinel(), &initial);
            let mut db = store.as_clarity_db(&TEST_HEADER_DB, &burn);
            db.initialize();
            db.begin();
            db.set_clarity_epoch_version(epoch).unwrap();
            db.commit().unwrap();
            let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, epoch);
            env.initialize_versioned_contract(id.clone(), ClarityVersion::Clarity2, source, None)
                .unwrap();
            for principal in &principals {
                env.stx_faucet(principal, 10_000_000);
            }
            let params = if name == "pox-4" {
                vec![0, 1, 10, 0]
            } else {
                vec![0, 1, 10, 25, 0]
            };
            let result = env
                .execute_transaction(
                    id.issuer.clone().into(),
                    None,
                    id.clone(),
                    "set-burnchain-parameters",
                    &symbols_from_values(params.into_iter().map(Value::UInt).collect()),
                )
                .unwrap()
                .0;
            assert_eq!(result, Value::okay_true());
            drop(env);
            store.commit_to_processed_block(&initial).unwrap();
        }
        // Execute a block of successful calls and pin all consensus state with
        // its commit root, in addition to each result, native event and balance.
        let mut calls = vec![
            (
                "delegate-stx",
                0usize,
                vec![
                    Value::UInt(1_000_000),
                    Value::Principal(principals[1].clone()),
                    Value::none(),
                    Value::none(),
                ],
            ),
            (
                "delegate-stack-stx",
                1,
                vec![
                    Value::Principal(principals[0].clone()),
                    Value::UInt(1_000_000),
                    pox_addr.clone(),
                    Value::UInt(0),
                    Value::UInt(2),
                ],
            ),
        ];
        let signer =
            StacksPublicKey::from_private(&StacksPrivateKey::from_slice(&[3; 32]).unwrap());
        if name == "pox-4" {
            calls.push((
                "set-signer-key-authorization",
                2,
                vec![
                    pox_addr.clone(),
                    Value::UInt(2),
                    Value::UInt(0),
                    Value::string_ascii_from_bytes(b"stack-stx".to_vec()).unwrap(),
                    Value::buff_from(signer.to_bytes_compressed()).unwrap(),
                    Value::Bool(true),
                    Value::UInt(1_000_000),
                    Value::UInt(1),
                ],
            ));
        }
        let mut solo = vec![
            Value::UInt(1_000_000),
            pox_addr,
            Value::UInt(0),
            Value::UInt(2),
        ];
        if name == "pox-4" {
            solo.extend([
                Value::none(),
                Value::buff_from(signer.to_bytes_compressed()).unwrap(),
                Value::UInt(1_000_000),
                Value::UInt(1),
            ]);
        }
        calls.push(("stack-stx", 2, solo));
        let tip = StacksBlockId([10; 32]);
        let mut store = marf.begin(&initial, &tip);
        for (function, sender, mut args) in calls {
            let db = store.as_clarity_db(&TEST_HEADER_DB, &burn);
            let mut env = OwnedEnvironment::new_cost_limited(
                false,
                CHAIN_ID_TESTNET,
                db,
                LimitedCostTracker::new_with_limit(epoch, ExecutionCost::max_value()),
                epoch,
            );
            if function == "delegate-stack-stx" || function == "stack-stx" {
                let height = env.eval_raw("burn-block-height").unwrap().0;
                let slot = if function == "stack-stx" { 2 } else { 3 };
                args[slot] = height;
            }
            if function == "set-signer-key-authorization" {
                let Value::UInt(height) = env.eval_raw("burn-block-height").unwrap().0 else {
                    panic!("height");
                };
                args[2] = Value::UInt(height / 10);
            }
            let (value, _, events) = env
                .execute_transaction(
                    principals[sender].clone(),
                    None,
                    id.clone(),
                    function,
                    &symbols_from_values(args),
                )
                .unwrap();
            assert!(
                matches!(&value, Value::Response(data) if data.committed),
                "{epoch:?}/{function}: {value}"
            );
            if function != "set-signer-key-authorization" {
                assert!(
                    !events.is_empty(),
                    "missing native PoX event for {function}"
                );
            }
            let cost = env.get_cost_total();
            let (mut db, tracker) = env.destruct().unwrap();
            db.begin();
            let balances: Vec<_> = principals
                .iter()
                .map(|principal| format!("{:?}", db.get_account_stx_balance(principal).unwrap()))
                .collect();
            db.roll_back().unwrap();
            drop(db);
            observations.insert(format!("{epoch:?}/{name}/{function}"), serde_json::json!({
                "value": value, "events": events.iter().enumerate().map(|(index, event)| event.json_serialize(index, &0u8, true).unwrap()).collect::<Vec<_>>(), "cost": cost, "memory": tracker.get_memory(), "balances": balances,
            }));
        }
        store.commit_to_processed_block(&tip).unwrap();
        let root = marf.get_marf().get_root_hash_at(&tip).unwrap();
        observations.insert(
            format!("{epoch:?}/{name}/state-root"),
            serde_json::json!(root.to_string()),
        );
    }
    if let Some(path) = std::env::var_os("CONTRACT_STORAGE_POX_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&observations).unwrap()).unwrap();
    }
    let expected: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(include_str!("contract_storage_pox_baseline.json")).unwrap();
    for (case, actual) in &observations {
        assert_eq!(Some(actual), expected.get(case), "{case}");
    }
    assert_eq!(observations.len(), expected.len());
}
