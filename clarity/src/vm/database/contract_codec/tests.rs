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

//! Compatibility guards for persisted executable records.

use super::*;
use crate::vm::callables::DefinedFunction;
use crate::vm::database::ClaritySerializable;

#[test]
fn header_roundtrip_rejects_wrong_envelopes_and_trailing_bytes() {
    let context = crate::vm::contexts::ContractContext::new(
        crate::vm::types::QualifiedContractIdentifier::local("header").unwrap(),
        crate::vm::ClarityVersion::Clarity2,
    );
    let (header, _) = crate::vm::database::contract_storage::split_contract(&context).unwrap();
    let bytes = encode_header(&header).unwrap();
    assert_eq!(
        serde_json::to_value(decode_header(&bytes).unwrap()).unwrap(),
        serde_json::to_value(&header).unwrap()
    );
    assert!(decode_function(&bytes).is_err());
    for end in 0..bytes.len() {
        assert!(decode_header(&bytes[..end]).is_err());
    }
    let mut corrupt = bytes;
    corrupt.push(0);
    assert!(decode_header(&corrupt).is_err());
}

#[test]
fn versioned_roundtrip_and_corrupt_envelopes() {
    let function = DefinedFunction::new(
        vec![],
        SymbolicExpression::atom_value(Value::UInt(u128::MAX)),
        DefineType::Public,
        &ClarityName::from_literal("run"),
        "ST000000000000000000002AMW42H.codec",
    );
    let record = FunctionDefinition::from(&function);
    let encoded = encode_function(&record).unwrap();
    assert_eq!(
        ClaritySerializable::serialize(&decode_function(&encoded).unwrap()),
        ClaritySerializable::serialize(&record)
    );
    for end in 0..encoded.len() {
        assert!(decode_function(&encoded[..end]).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(decode_function(&trailing).is_err());
    let mut future = encoded;
    future[4] = 2;
    assert!(decode_function(&future).is_err());
}

use std::collections::BTreeMap;

use stacks_common::util::hash::{hex_bytes, to_hex};

use crate::vm::ClarityVersion;
use crate::vm::contexts::ContractContext;
use crate::vm::database::contract_storage::split_contract;
use crate::vm::database::{
    DataMapMetadata, DataVariableMetadata, FungibleTokenMetadata, NonFungibleTokenMetadata,
};
use crate::vm::types::signatures::{
    CallableSubtype, FunctionSignature, ListTypeData, SequenceSubtype, StringSubtype,
    TupleTypeSignature,
};
use crate::vm::types::{CallableData, PrincipalData, QualifiedContractIdentifier, TupleData};

/// Deliberately cover nested serde variants, not only the explicit outer AST.
/// Every unordered collection has one entry so byte equality does not depend on
/// HashMap/HashSet iteration order. Tuple fields and unions have stable ordering.
fn golden_records() -> (ContractHeader, FunctionDefinition) {
    let id = QualifiedContractIdentifier::local("codec-fixture").unwrap();
    let name = ClarityName::from_literal("entry");
    let trait_id = TraitIdentifier {
        name: ClarityName::from_literal("trait"),
        contract_identifier: id.clone(),
    };
    let tuple_type =
        TupleTypeSignature::try_from(vec![(name.clone(), TypeSignature::UIntType)]).unwrap();
    let types = vec![
        TypeSignature::NoType,
        TypeSignature::IntType,
        TypeSignature::UIntType,
        TypeSignature::BoolType,
        TypeSignature::SequenceType(SequenceSubtype::BufferType(17u32.try_into().unwrap())),
        TypeSignature::SequenceType(SequenceSubtype::ListType(
            ListTypeData::new_list(TypeSignature::IntType, 3).unwrap(),
        )),
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(
            19u32.try_into().unwrap(),
        ))),
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(
            23u32.try_into().unwrap(),
        ))),
        TypeSignature::PrincipalType,
        TypeSignature::TupleType(tuple_type),
        TypeSignature::OptionalType(Box::new(TypeSignature::BoolType)),
        TypeSignature::ResponseType(Box::new((TypeSignature::UIntType, TypeSignature::IntType))),
        TypeSignature::CallableType(CallableSubtype::Principal(id.clone())),
        TypeSignature::CallableType(CallableSubtype::Trait(trait_id.clone())),
        TypeSignature::ListUnionType(
            [
                CallableSubtype::Principal(id.clone()),
                CallableSubtype::Trait(trait_id.clone()),
            ]
            .into_iter()
            .collect(),
        ),
        TypeSignature::TraitReferenceType(trait_id.clone()),
    ];
    let values = vec![
        Value::Int(i128::MIN),
        Value::UInt(u128::MAX),
        Value::Bool(true),
        Value::Bool(false),
        Value::buff_from(vec![0, 128, 255]).unwrap(),
        Value::cons_list_unsanitized(vec![Value::Int(-1), Value::Int(2)]).unwrap(),
        Value::string_ascii_from_bytes(b"ascii".to_vec()).unwrap(),
        Value::string_utf8_from_bytes("λ🦀".as_bytes().to_vec()).unwrap(),
        Value::Principal(PrincipalData::Standard(id.issuer.clone())),
        Value::Principal(PrincipalData::Contract(id.clone())),
        Value::Tuple(TupleData::from_data(vec![(name.clone(), Value::UInt(9))]).unwrap()),
        Value::none(),
        Value::some(Value::Bool(true)).unwrap(),
        Value::okay(Value::UInt(7)).unwrap(),
        Value::error(Value::Int(-8)).unwrap(),
        Value::CallableContract(CallableData {
            contract_identifier: id.clone(),
            trait_identifier: None,
        }),
        Value::CallableContract(CallableData {
            contract_identifier: id.clone(),
            trait_identifier: Some(Box::new(trait_id.clone())),
        }),
    ];
    let mut body: Vec<_> = values
        .iter()
        .cloned()
        .map(SymbolicExpression::atom_value)
        .collect();
    body.extend([
        SymbolicExpression::atom(name.clone()),
        SymbolicExpression::literal_value(Value::UInt(42)),
        SymbolicExpression::field(trait_id.clone()),
        SymbolicExpression::trait_reference(
            name.clone(),
            TraitDefinition::Defined(trait_id.clone()),
        ),
        SymbolicExpression::trait_reference(
            name.clone(),
            TraitDefinition::Imported(trait_id.clone()),
        ),
    ]);
    let mut body = SymbolicExpression::list(body);
    body.id = 257;
    #[cfg(feature = "developer-mode")]
    {
        body.span = Span {
            start_line: 3,
            start_column: 5,
            end_line: 8,
            end_column: 13,
        };
        body.pre_comments = vec![("before".into(), body.span.clone())];
        body.end_line_comment = Some("inline".into());
        body.post_comments = vec![("after".into(), body.span.clone())];
    }
    let function = DefinedFunction::new(vec![], body, DefineType::Public, &name, &id.to_string());
    let mut record = FunctionDefinition::from(&function);
    record.arg_types = types.clone();
    record.arguments = (0..types.len())
        .map(|n| format!("a{n}").try_into().unwrap())
        .collect();

    let mut context = ContractContext::new(id, ClarityVersion::Clarity7);
    let constant = Value::Tuple(
        TupleData::from_data(
            values
                .into_iter()
                .enumerate()
                .map(|(n, value)| (format!("v{n}").try_into().unwrap(), value))
                .collect(),
        )
        .unwrap(),
    );
    context
        .shared_mut()
        .variables
        .insert(name.clone(), constant);
    context
        .functions
        .insert(name.clone(), record.clone().into_function());
    context.shared_mut().defined_traits.insert(
        name.clone(),
        BTreeMap::from([(
            name.clone(),
            FunctionSignature {
                args: types,
                returns: TypeSignature::BoolType,
            },
        )]),
    );
    context.shared_mut().implemented_traits.insert(trait_id);
    context.shared_mut().persisted_names.insert(name.clone());
    context.shared_mut().meta_data_map.insert(
        name.clone(),
        DataMapMetadata {
            key_type: TypeSignature::IntType,
            value_type: TypeSignature::UIntType,
        },
    );
    context.shared_mut().meta_data_var.insert(
        name.clone(),
        DataVariableMetadata {
            value_type: TypeSignature::BoolType,
        },
    );
    context.shared_mut().meta_nft.insert(
        name.clone(),
        NonFungibleTokenMetadata {
            key_type: TypeSignature::PrincipalType,
        },
    );
    context.shared_mut().meta_ft.insert(
        name,
        FungibleTokenMetadata {
            total_supply: Some(u128::MAX),
        },
    );
    context.shared_mut().data_size = 16385;
    let (mut header, _) = split_contract(&context).unwrap();
    // Fix the stored flag independently of later sanitizer implementation changes.
    header.constants_sanitized = false;
    header.functions.get_mut(0).unwrap().dependencies = vec![0];
    (header, record)
}

/// These bytes are the V1 header compatibility contract. Do not regenerate them to
/// accommodate a serde/schema change: introduce a versioned reader/writer instead.
#[test]
fn golden_v1_header_bytes() {
    let (header, _) = golden_records();
    let bytes = hex_bytes(include_str!("fixtures/header-v1.hex").trim()).unwrap();
    assert_eq!(to_hex(&encode_header(&header).unwrap()), to_hex(&bytes));
    assert_eq!(
        serde_json::to_value(decode_header(&bytes).unwrap()).unwrap(),
        serde_json::to_value(header).unwrap()
    );
    let mut old_envelope = bytes;
    old_envelope[4] = 2;
    assert!(decode_header(&old_envelope).is_err());
}

#[test]
fn header_bytes_are_independent_of_hash_seeds_and_insertion_order() {
    let (original, _) = golden_records();
    let mut reference = None;
    for reverse in [false, true].into_iter().cycle().take(32) {
        let mut header = original.clone();
        let shared = Arc::make_mut(&mut header.shared);
        let names = if reverse {
            ["c", "b", "a"]
        } else {
            ["a", "b", "c"]
        };
        shared.variables = HashMap::new();
        shared.persisted_names = HashSet::new();
        for name in names {
            let name = ClarityName::from_literal(name);
            shared.variables.insert(name.clone(), Value::UInt(42));
            shared.persisted_names.insert(name);
        }
        let bytes = encode_header(&header).unwrap();
        if let Some(reference) = &reference {
            assert_eq!(&bytes, reference);
        } else {
            reference = Some(bytes);
        }
    }
}

#[test]
fn golden_v1_function_bytes_and_cross_build_diagnostics() {
    let (_, function) = golden_records();
    let plain = hex_bytes(include_str!("fixtures/function-v1.hex").trim()).unwrap();
    let developer = hex_bytes(include_str!("fixtures/function-developer-v1.hex").trim()).unwrap();
    let expected = if cfg!(feature = "developer-mode") {
        &developer
    } else {
        &plain
    };
    assert_eq!(encode_function(&function).unwrap(), *expected);
    assert_eq!(
        ClaritySerializable::serialize(&decode_function(expected).unwrap()),
        ClaritySerializable::serialize(&function)
    );
    for bytes in [&plain, &developer] {
        let decoded = decode_function(bytes).unwrap();
        assert_eq!(decoded.body.expr, function.body.expr);
        assert_eq!(decoded.body.id, function.body.id);
        assert_eq!(decoded.arg_types, function.arg_types);
    }
}

#[test]
fn golden_small_discriminants() {
    assert_eq!(
        postcard::to_allocvec(&[
            DefineType::ReadOnly,
            DefineType::Public,
            DefineType::Private
        ])
        .unwrap(),
        [0, 1, 2]
    );
    assert_eq!(
        postcard::to_allocvec(&[
            ClarityVersion::Clarity1,
            ClarityVersion::Clarity2,
            ClarityVersion::Clarity3,
            ClarityVersion::Clarity4,
            ClarityVersion::Clarity5,
            ClarityVersion::Clarity6,
            ClarityVersion::Clarity7
        ])
        .unwrap(),
        [0, 1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn json_remains_a_strict_number_array() {
    use crate::vm::types::{ASCIIData, BuffData};
    for text in ["{\"data\":[]}", "{\"data\":[0,1,127,128,255]}"] {
        let buffer: BuffData = serde_json::from_str(text).unwrap();
        let ascii: ASCIIData = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&buffer).unwrap(), text);
        assert_eq!(serde_json::to_string(&ascii).unwrap(), text);
    }
    for text in [
        "{\"data\":[256]}",
        "{\"data\":[-1]}",
        "{\"data\":[1.5]}",
        "{\"data\":\"abc\"}",
        "{\"data\":null}",
    ] {
        assert!(serde_json::from_str::<BuffData>(text).is_err());
        assert!(serde_json::from_str::<ASCIIData>(text).is_err());
    }
}
