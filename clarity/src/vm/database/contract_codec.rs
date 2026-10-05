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

//! Versioned Postcard executable records, including shared headers. This codec
//! constructs ordinary VM types and leaves consensus accounting unchanged.
//! Ordinary non-executable metadata retains its existing text representation.
//! Diagnostic presence is explicit, so either build can read the other's data.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use postcard::ser_flavors::Flavor;
use serde::{Deserialize, Serialize};

use super::contract_storage::ContractHeader;
use super::{
    DataMapMetadata, DataVariableMetadata, FungibleTokenMetadata, NonFungibleTokenMetadata,
};
use crate::vm::ClarityVersion;
use crate::vm::callables::{DefineType, FunctionDefinition, FunctionIdentifier};
use crate::vm::contexts::ContractSharedContext;
use crate::vm::errors::{VmExecutionError, VmInternalError};
use crate::vm::representations::{
    ClarityName, Span, SymbolicExpression, SymbolicExpressionType, TraitDefinition,
};
use crate::vm::types::signatures::FunctionSignature;
use crate::vm::types::{QualifiedContractIdentifier, TraitIdentifier, TypeSignature, Value};

/// Envelopes distinguish record kinds and freeze their binary field order.
const FUNCTION_MAGIC: &[u8] = b"SCFN\x01\0\0\0";
const HEADER_MAGIC: &[u8] = b"SCHD\x01\0\0\0";

/// Field/variant order, including the derived serde of nested Value,
/// TypeSignature and metadata types, is part of the disk schema. Run the fixed-byte
/// fixtures in contract_codec/tests.rs when changing them; incompatible layouts
/// require a new envelope and migration.
#[derive(Serialize, Deserialize)]
#[serde(remote = "FunctionDefinition")]
struct Function {
    identifier: FunctionIdentifier,
    name: ClarityName,
    arg_types: Vec<TypeSignature>,
    define_type: DefineType,
    arguments: Vec<ClarityName>,
    #[serde(with = "node")]
    body: SymbolicExpression,
}

/// Persist only the shared VM fields, borrowing them during encoding. The remote
/// derive constructs ContractSharedContext exhaustively: adding a VM field requires
/// an explicit storage decision here, including fields deliberately skipped.
#[derive(Serialize, Deserialize)]
#[serde(remote = "ContractSharedContext")]
struct Shared {
    contract_identifier: QualifiedContractIdentifier,
    #[serde(serialize_with = "ordered_map")]
    variables: HashMap<ClarityName, Value>,
    #[serde(skip)]
    function_names: Arc<[ClarityName]>,
    #[serde(serialize_with = "ordered_map")]
    defined_traits: HashMap<ClarityName, BTreeMap<ClarityName, FunctionSignature>>,
    #[serde(serialize_with = "ordered_set")]
    implemented_traits: HashSet<TraitIdentifier>,
    #[serde(serialize_with = "ordered_set")]
    persisted_names: HashSet<ClarityName>,
    #[serde(serialize_with = "ordered_map")]
    meta_data_map: HashMap<ClarityName, DataMapMetadata>,
    #[serde(serialize_with = "ordered_map")]
    meta_data_var: HashMap<ClarityName, DataVariableMetadata>,
    #[serde(serialize_with = "ordered_map")]
    meta_nft: HashMap<ClarityName, NonFungibleTokenMetadata>,
    #[serde(serialize_with = "ordered_map")]
    meta_ft: HashMap<ClarityName, FungibleTokenMetadata>,
    data_size: u64,
    clarity_version: ClarityVersion,
    #[serde(skip)]
    is_deploying: bool,
}

// Ordering is a write-time concern. Reads still construct the VM's hash tables.
fn ordered_map<K: Serialize + Ord, V: Serialize, S: serde::Serializer>(
    value: &HashMap<K, V>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value
        .iter()
        .collect::<BTreeMap<_, _>>()
        .serialize(serializer)
}

fn ordered_set<T: Serialize + Ord, S: serde::Serializer>(
    value: &HashSet<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value.iter().collect::<BTreeSet<_>>().serialize(serializer)
}

pub(crate) mod shared {
    use std::sync::Arc;

    use crate::vm::contexts::ContractSharedContext;

    pub fn serialize<S: serde::Serializer>(
        value: &Arc<ContractSharedContext>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        super::Shared::serialize(value, serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<ContractSharedContext>, D::Error> {
        super::Shared::deserialize(deserializer).map(Arc::new)
    }
}

/// Explicit AST variants avoid JSON's conditionally omitted diagnostic fields.
#[derive(Serialize, Deserialize)]
#[serde(remote = "SymbolicExpressionType")]
enum Expr {
    AtomValue(Value),
    Atom(ClarityName),
    List(#[serde(with = "nodes")] Vec<SymbolicExpression>),
    LiteralValue(Value),
    Field(TraitIdentifier),
    TraitReference(ClarityName, TraitDefinition),
}

/// Optional diagnostic side data has the same schema in every feature build.
#[derive(Default, Serialize, Deserialize)]
struct Diagnostics {
    span: Span,
    pre_comments: Vec<(String, Span)>,
    end_line_comment: Option<String>,
    post_comments: Vec<(String, Span)>,
}

mod node {
    use super::*;

    #[derive(Serialize)]
    struct BorrowedExpr<'a>(#[serde(with = "Expr")] &'a SymbolicExpressionType);
    #[derive(Deserialize)]
    struct OwnedExpr(#[serde(with = "Expr")] SymbolicExpressionType);

    pub fn serialize<S: serde::Serializer>(
        node: &SymbolicExpression,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        #[cfg(feature = "developer-mode")]
        let diagnostics = Some(Diagnostics {
            span: node.span.clone(),
            pre_comments: node.pre_comments.clone(),
            end_line_comment: node.end_line_comment.clone(),
            post_comments: node.post_comments.clone(),
        });
        #[cfg(not(feature = "developer-mode"))]
        let diagnostics: Option<Diagnostics> = None;
        (BorrowedExpr(&node.expr), node.id, diagnostics).serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SymbolicExpression, D::Error> {
        let (expr, id, diagnostics): (OwnedExpr, u64, Option<Diagnostics>) =
            Deserialize::deserialize(deserializer)?;
        let mut node = SymbolicExpression::atom_value(Value::Bool(false));
        node.expr = expr.0;
        node.id = id;
        #[cfg(feature = "developer-mode")]
        {
            let diagnostics = diagnostics.unwrap_or_default();
            node.span = diagnostics.span;
            node.pre_comments = diagnostics.pre_comments;
            node.end_line_comment = diagnostics.end_line_comment;
            node.post_comments = diagnostics.post_comments;
        }
        #[cfg(not(feature = "developer-mode"))]
        let _ = diagnostics;
        Ok(node)
    }
    #[cfg(test)]
    #[test]
    fn diagnostics_presence_is_readable_in_either_feature_build() {
        for present in [false, true] {
            let diagnostics = present.then(|| Diagnostics {
                span: Span {
                    start_line: 7,
                    start_column: 1,
                    end_line: 8,
                    end_column: 2,
                },
                end_line_comment: Some("comment".into()),
                ..Diagnostics::default()
            });
            let expr = SymbolicExpressionType::Atom(ClarityName::from_literal("name"));
            let bytes = postcard::to_allocvec(&(BorrowedExpr(&expr), 42u64, diagnostics)).unwrap();
            let mut reader = postcard::Deserializer::from_bytes(&bytes);
            let node = deserialize(&mut reader).unwrap();
            assert_eq!(node.id, 42);
            assert_eq!(node.expr, expr);
            assert!(reader.finalize().unwrap().is_empty());
            #[cfg(feature = "developer-mode")]
            {
                assert_eq!(node.span.start_line, if present { 7 } else { 0 });
            }
        }
    }
}

mod nodes {
    use serde::ser::SerializeSeq;

    use super::*;
    #[derive(Serialize)]
    struct Borrowed<'a>(#[serde(with = "node")] &'a SymbolicExpression);
    #[derive(Deserialize)]
    struct Owned(#[serde(with = "node")] SymbolicExpression);

    pub fn serialize<S: serde::Serializer>(
        nodes: &[SymbolicExpression],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(nodes.len()))?;
        for node in nodes {
            seq.serialize_element(&Borrowed(node))?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<SymbolicExpression>, D::Error> {
        Ok(Vec::<Owned>::deserialize(deserializer)?
            .into_iter()
            .map(|v| v.0)
            .collect())
    }
}

fn invalid(error: impl std::fmt::Display) -> VmExecutionError {
    VmInternalError::Expect(format!("Invalid Postcard contract record: {error}")).into()
}

/// Encode a function with a version envelope, preserving all available diagnostics.
pub fn encode_function(record: &FunctionDefinition) -> Result<Vec<u8>, VmExecutionError> {
    #[derive(Serialize)]
    struct Borrowed<'a>(#[serde(with = "Function")] &'a FunctionDefinition);
    encode_value(FUNCTION_MAGIC, &Borrowed(record))
}

fn encode_value(magic: &[u8], value: &impl Serialize) -> Result<Vec<u8>, VmExecutionError> {
    let mut serializer = postcard::Serializer {
        output: postcard::ser_flavors::AllocVec::new(),
    };
    serializer.output.try_extend(magic).map_err(invalid)?;
    #[cfg(not(target_family = "wasm"))]
    value
        .serialize(serde_stacker::Serializer::new(&mut serializer))
        .map_err(invalid)?;
    #[cfg(target_family = "wasm")]
    value.serialize(&mut serializer).map_err(invalid)?;
    serializer.output.finalize().map_err(invalid)
}

/// Decode directly into VM objects. Unknown envelopes and trailing bytes fail.
pub fn decode_function(bytes: &[u8]) -> Result<FunctionDefinition, VmExecutionError> {
    #[derive(Deserialize)]
    struct Owned(#[serde(with = "Function")] FunctionDefinition);
    Ok(decode_value::<Owned>(FUNCTION_MAGIC, bytes)?.0)
}

fn decode_value<T: serde::de::DeserializeOwned>(
    magic: &[u8],
    bytes: &[u8],
) -> Result<T, VmExecutionError> {
    let bytes = bytes
        .strip_prefix(magic)
        .ok_or_else(|| invalid("unsupported envelope"))?;
    let mut deserializer = postcard::Deserializer::from_bytes(bytes);
    #[cfg(not(target_family = "wasm"))]
    let record =
        T::deserialize(serde_stacker::Deserializer::new(&mut deserializer)).map_err(invalid)?;
    #[cfg(target_family = "wasm")]
    let record = T::deserialize(&mut deserializer).map_err(invalid)?;
    if !deserializer.finalize().map_err(invalid)?.is_empty() {
        return Err(invalid("trailing bytes"));
    }
    Ok(record)
}

/// Encode the complete shared context and dependency directory as Postcard.
pub fn encode_header(header: &ContractHeader) -> Result<Vec<u8>, VmExecutionError> {
    encode_value(HEADER_MAGIC, header)
}

/// Decode a shared header without intermediate JSON or collection conversion.
pub fn decode_header(bytes: &[u8]) -> Result<ContractHeader, VmExecutionError> {
    let header: ContractHeader = decode_value(HEADER_MAGIC, bytes)?;
    header.validate_directory()?;
    Ok(header)
}

#[cfg(test)]
mod tests;
