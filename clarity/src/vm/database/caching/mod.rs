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

use stacks_common::types::StacksEpochId;
use weight_limited_fifo::WeightLimitedFifo;

use crate::vm::callables::DefinedFunction;
use crate::vm::database::contract_storage::LoadedContractHeader;
use crate::vm::representations::ClarityName;
use crate::vm::types::QualifiedContractIdentifier;

mod weight_limited_fifo;

/// Bound per-transaction retention across repeated contract calls. Five MiB keeps
/// common call sets reusable without retaining an unbounded contract working set.
/// The budget counts encoded bytes, not decoded heap, independently of consensus charges.
pub const DEFAULT_CONTRACT_CACHE_BYTE_LIMIT: u64 = 5 * 1024 * 1024;

pub(crate) type ContractCacheKey = (
    QualifiedContractIdentifier,
    StacksEpochId,
    ContractCachePart,
);

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) enum ContractCachePart {
    Header,
    Function(ClarityName),
}

#[derive(Clone)]
pub(crate) enum CachedContractPart {
    Header(LoadedContractHeader),
    Function(DefinedFunction),
}

/// One budget and eviction queue for all independently reusable contract parts, counted once.
pub struct ContractCache {
    entries: WeightLimitedFifo<ContractCacheKey, CachedContractPart>,
}

impl ContractCache {
    pub fn new(byte_limit: u64) -> Self {
        Self {
            entries: WeightLimitedFifo::new(byte_limit),
        }
    }

    pub fn hits(&self) -> u64 {
        self.entries.hits()
    }
    pub fn misses(&self) -> u64 {
        self.entries.misses()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn total_weight(&self) -> u64 {
        self.entries.total_weight()
    }
    pub fn weight_limit(&self) -> u64 {
        self.entries.weight_limit()
    }

    pub(crate) fn get(&mut self, key: &ContractCacheKey) -> Option<&CachedContractPart> {
        self.entries.get(key)
    }

    pub(crate) fn insert(&mut self, key: ContractCacheKey, value: CachedContractPart, weight: u64) {
        self.entries.insert(key, value, weight);
    }
}

/// Execution caches belong to a single transaction and never retain mutable VM state.
pub struct ClarityExecutionCache {
    pub contracts: ContractCache,
}

impl Default for ClarityExecutionCache {
    fn default() -> Self {
        Self {
            contracts: ContractCache::new(DEFAULT_CONTRACT_CACHE_BYTE_LIMIT),
        }
    }
}
