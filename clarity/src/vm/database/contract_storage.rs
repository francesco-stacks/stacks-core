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

//! Versioned executable contract records.
//!
//! Code, derived analysis, and mutable chainstate are separate. Future access-set
//! analysis belongs in its own metadata namespace. Row lengths bound the encoded
//! cache budget; consensus cost and memory accounting retain
//! the original source-length-plus-constant-data size in every epoch.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Serialize;

#[cfg(any(test, feature = "testing"))]
use super::ClaritySerializable;
use super::clarity_store::MetadataValue;
use crate::vm::callables::{DefinedFunction, FunctionDefinition};
use crate::vm::contexts::{ContractContext, ContractSharedContext};
use crate::vm::errors::{VmExecutionError, VmInternalError};
use crate::vm::functions::lookup_reserved_functions;
use crate::vm::representations::{ClarityName, SymbolicExpression};
use crate::vm::version::ClarityVersion;

/// Shared record key; its presence also identifies an initialized contract.
pub const CONTRACT_HEADER_KEY: &str = "vm-metadata::9::contract-header";
/// Historical full-contract key used by migration, the metadata RPC, and baselines.
pub const LEGACY_CONTRACT_KEY: &str = "vm-metadata::9::contract";
pub const FUNCTION_PREFIX: &str = "vm-metadata::9::contract-function::";

pub fn function_key(name: &str) -> String {
    format!("{FUNCTION_PREFIX}{name}")
}

fn invalid(message: impl Into<String>) -> VmExecutionError {
    VmInternalError::Expect(message.into()).into()
}

impl MetadataValue {
    pub fn decode_header(&self) -> Result<ContractHeader, VmExecutionError> {
        match self {
            Self::Text(_) => Err(invalid("Executable header must be a BLOB")),
            Self::Blob(bytes) => super::contract_codec::decode_header(bytes),
        }
    }

    pub fn decode_function(&self) -> Result<FunctionDefinition, VmExecutionError> {
        let value = match self {
            Self::Text(_) => return Err(invalid("Executable function must be a BLOB")),
            Self::Blob(bytes) => super::contract_codec::decode_function(bytes)?,
        };
        Ok(value)
    }
}

/// The directory stores direct local dependencies; weights are not persisted.
#[derive(Clone, Serialize, Deserialize)]
pub struct FunctionEntry {
    pub name: ClarityName,
    pub dependencies: Vec<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ContractHeader {
    #[serde(with = "super::contract_codec::shared")]
    pub shared: Arc<ContractSharedContext>,
    pub functions: Vec<FunctionEntry>,
    /// Reserved for a validated no-op sanitizer result. Writers currently set
    /// false and readers always perform the normal epoch-specific sanitization.
    pub constants_sanitized: bool,
}

impl ContractHeader {
    pub(crate) fn validate_directory(&self) -> Result<(), VmExecutionError> {
        if self
            .functions
            .windows(2)
            .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err(invalid("Contract directory is not strictly sorted"));
        }
        for entry in &self.functions {
            if entry
                .dependencies
                .iter()
                .any(|index| *index as usize >= self.functions.len())
            {
                return Err(invalid("Contract directory references an absent function"));
            }
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
impl From<&DefinedFunction> for FunctionDefinition {
    fn from(function: &DefinedFunction) -> Self {
        function.0.as_ref().clone()
    }
}

impl FunctionDefinition {
    pub fn into_function(self) -> DefinedFunction {
        DefinedFunction(Arc::new(self))
    }
}

#[cfg(any(test, feature = "testing"))]
impl ClaritySerializable for ContractHeader {
    fn serialize(&self) -> String {
        serde_json::to_string(self).expect("Failed to serialize contract header")
    }
}

#[cfg(any(test, feature = "testing"))]
impl ClaritySerializable for FunctionDefinition {
    fn serialize(&self) -> String {
        serde_json::to_string(self).expect("Failed to serialize contract function")
    }
}

/// Convert a fully initialized context. Neither initialization nor analysis is rerun.
pub fn split_contract(
    contract: &ContractContext,
) -> Result<(ContractHeader, BTreeMap<ClarityName, DefinedFunction>), VmExecutionError> {
    if !contract.function_names.is_empty()
        && (contract.function_names.len() != contract.functions.len()
            || contract
                .function_names
                .iter()
                .any(|name| !contract.functions.contains_key(name)))
    {
        return Err(invalid("Cannot save a partially loaded contract"));
    }
    let records: BTreeMap<_, _> = contract
        .functions
        .iter()
        .map(|(name, function)| (name.clone(), function.clone()))
        .collect();
    let mut directory: Vec<_> = records
        .keys()
        .map(|name| FunctionEntry {
            name: name.clone(),
            dependencies: Vec::new(),
        })
        .collect();
    let names: Vec<_> = records.keys().cloned().collect();
    for (index, function) in records.values().enumerate() {
        directory[index].dependencies =
            local_dependencies(&function.body, contract.get_clarity_version(), &names)
                .into_iter()
                .collect();
    }
    let header = ContractHeader {
        shared: contract.shared.clone(),
        functions: directory,
        constants_sanitized: false,
    };
    Ok((header, records))
}

/// Conservatively include every non-reserved local-function atom, regardless of
/// syntactic position. The exclusion must mirror `vm::lookup_function`: native
/// functions take precedence over local names; reserved variables do not. Direct
/// contract-call entrypoints select the named definition without this exclusion.
/// This covers call heads and higher-order native callbacks
/// without maintaining a second list of the VM's special forms. Extra bodies
/// may cost CPU/heap, but never change consensus charges. Runtime function names
/// are atoms; adding computed function names would require revisiting this rule.
fn local_dependencies(
    body: &SymbolicExpression,
    version: &ClarityVersion,
    names: &[ClarityName],
) -> BTreeSet<u32> {
    let mut dependencies = BTreeSet::new();
    let mut pending = vec![body];
    while let Some(expression) = pending.pop() {
        if let Some(name) = expression.match_atom()
            && let Some(index) = function_index(names, name)
            && lookup_reserved_functions(name, version).is_none()
        {
            dependencies.insert(index as u32);
        }
        if let Some(children) = expression.match_list() {
            pending.extend(children);
        }
    }
    dependencies
}

pub(crate) fn function_index(names: &[ClarityName], name: &str) -> Option<usize> {
    names
        .binary_search_by(|entry| entry.as_str().cmp(name))
        .ok()
}

