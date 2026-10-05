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

//! Shared inputs for independent full-JSON and selective-loading comparisons.
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value};

use crate::chainstate::stacks::boot::*;

pub fn sample_argument(ty: &clarity::vm::types::TypeSignature, nonzero: bool) -> Value {
    use clarity::vm::types::signatures::{SequenceSubtype as S, StringSubtype};
    use clarity::vm::types::{TupleData, TypeSignature as T};
    match ty {
        T::UIntType => Value::UInt(u128::from(nonzero)),
        T::IntType => Value::Int(i128::from(nonzero)),
        T::BoolType => Value::Bool(nonzero),
        T::PrincipalType => Value::Principal(
            QualifiedContractIdentifier::local("caller")
                .unwrap()
                .issuer
                .into(),
        ),
        T::OptionalType(inner) if nonzero => Value::some(sample_argument(inner, nonzero)).unwrap(),
        T::OptionalType(_) => Value::none(),
        T::ResponseType(inner) => Value::okay(sample_argument(&inner.0, nonzero)).unwrap(),
        T::TupleType(tuple) => Value::Tuple(
            TupleData::from_data(
                tuple
                    .get_type_map()
                    .iter()
                    .map(|(name, ty)| (name.clone(), sample_argument(ty, nonzero)))
                    .collect(),
            )
            .unwrap(),
        ),
        T::SequenceType(S::BufferType(len)) => {
            Value::buff_from(vec![
                u8::from(nonzero);
                if nonzero { u32::from(len) as usize } else { 0 }
            ])
            .unwrap()
        }
        T::SequenceType(S::ListType(list)) => {
            Value::cons_list_unsanitized(if nonzero && list.get_max_len() > 0 {
                vec![sample_argument(list.get_list_item_type(), nonzero)]
            } else {
                vec![]
            })
            .unwrap()
        }
        T::SequenceType(S::StringType(StringSubtype::ASCII(_))) => {
            Value::string_ascii_from_bytes(vec![]).unwrap()
        }
        T::SequenceType(S::StringType(StringSubtype::UTF8(_))) => {
            Value::string_utf8_from_bytes(vec![]).unwrap()
        }
        other => panic!("add a sample for boot argument type {other:?}"),
    }
}

pub fn boot_contracts() -> [(&'static str, &'static str, ClarityVersion); 11] {
    [
        ("pox-4", POX_4_CODE.as_str(), ClarityVersion::Clarity2),
        ("bns", BOOT_CODE_BNS, ClarityVersion::Clarity1),
        ("costs", BOOT_CODE_COSTS, ClarityVersion::Clarity1),
        ("costs-2", BOOT_CODE_COSTS_2, ClarityVersion::Clarity1),
        (
            "costs-2-testnet",
            BOOT_CODE_COSTS_2_TESTNET,
            ClarityVersion::Clarity1,
        ),
        ("costs-3", BOOT_CODE_COSTS_3, ClarityVersion::Clarity1),
        ("costs-4", BOOT_CODE_COSTS_4, ClarityVersion::Clarity1),
        ("signers", SIGNERS_BODY, ClarityVersion::Clarity2),
        ("signers-0-xxx", SIGNERS_DB_0_BODY, ClarityVersion::Clarity2),
        ("signers-1-xxx", SIGNERS_DB_1_BODY, ClarityVersion::Clarity2),
        (
            "signers-voting",
            SIGNERS_VOTING_BODY,
            ClarityVersion::Clarity2,
        ),
    ]
}

/// Regenerate only with unchanged HEAD's writer. Copy this module into the
/// reference checkout, register it, then run this ignored test with
/// CONTRACT_STORAGE_LEGACY_OUTPUT set to a new JSON path.
#[test]
#[ignore = "writes reference fixtures; requires the original JSON implementation"]
fn export_original_contract_json() {
    use clarity::vm::contexts::OwnedEnvironment;
    use clarity::vm::database::{ClarityBackingStore, MemoryBackingStore};
    use stacks_common::consts::CHAIN_ID_TESTNET;
    use stacks_common::types::StacksEpochId;
    let output = std::env::var_os("CONTRACT_STORAGE_LEGACY_OUTPUT").unwrap();
    let mut store = MemoryBackingStore::new();
    let mut db = store.as_clarity_db();
    db.begin();
    db.set_clarity_epoch_version(StacksEpochId::Epoch40)
        .unwrap();
    db.commit().unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, StacksEpochId::Epoch40);
    for (name, version, source) in [
        ("legacy-rich", ClarityVersion::Clarity1, "
            (define-constant extremes {signed: -170141183460469231731687303715884105728, unsigned: u340282366920938463463374607431768211455})
            (define-trait interface ((run () (response uint uint))))
            (impl-trait .legacy-rich.interface)
            (define-map entries uint (tuple (n int)))
            (define-data-var counter uint u0)
            (define-fungible-token token u100)
            (define-non-fungible-token nft uint)
            (define-public (run) (ok (get unsigned extremes)))"),
        ("legacy-rpc", ClarityVersion::Clarity2, "
            (define-constant extremes {signed: -170141183460469231731687303715884105728, unsigned: u340282366920938463463374607431768211455})
            (define-read-only (run) extremes)"),
    ] {
        env.initialize_versioned_contract(QualifiedContractIdentifier::local(name).unwrap(), version, source, None).unwrap();
    }
    drop(env);
    let conn = store.get_side_store();
    let data: Vec<(String, String)> = conn
        .prepare("SELECT key,value FROM data_table ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // Asking for TEXT also prevents accidentally generating these fixtures from
    // the binary writer in the modified checkout.
    let metadata: Vec<(String, String, String)> = conn
        .prepare("SELECT key,blockhash,value FROM metadata_table ORDER BY key,blockhash")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        metadata
            .iter()
            .filter(|(key, _, _)| key.ends_with("::contract"))
            .count(),
        2
    );
    let fixture = serde_json::json!({"writer_commit": "3fd5d990f4d53562dd1def3eb7849ae801c2d807", "data": data, "metadata": metadata});
    use std::io::Write;
    let mut file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(output)
        .unwrap();
    file.write_all(&serde_json::to_vec_pretty(&fixture).unwrap())
        .unwrap();
}
