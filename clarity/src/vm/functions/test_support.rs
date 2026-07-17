// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

//! Shared test environment factory for direct special-function tests.
//!
//! `GlobalContext` borrows the backing store and `ExecutionState` /
//! `InvocationContext` borrow further down the stack, so a ready environment
//! cannot be returned from a fixture; the factory owns the store and `run`
//! builds the borrow stack, handing the test body ready-to-use references.

use rstest::fixture;
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::StacksEpochId;

use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::LimitedCostTracker;
use crate::vm::database::MemoryBackingStore;
use crate::vm::types::QualifiedContractIdentifier;
use crate::vm::{CallStack, ClarityVersion, ContractContext, GlobalContext, LocalContext};

pub struct SpecialFnEnvFactory(MemoryBackingStore);

/// rstest fixture; also directly callable (`let mut env = special_fn_env();`)
/// inside `#[apply(test_clarity_versions)]` tests.
#[fixture]
pub fn special_fn_env() -> SpecialFnEnvFactory {
    SpecialFnEnvFactory(MemoryBackingStore::new())
}

impl SpecialFnEnvFactory {
    /// Build a fresh environment and run `body` in a writable frame.
    /// `setup_contract` customizes the transient `ContractContext`
    /// (e.g. inserting FT/NFT/map/var metadata) before execution.
    pub fn run<R>(
        &mut self,
        version: ClarityVersion,
        epoch: StacksEpochId,
        setup_contract: impl FnOnce(&mut ContractContext),
        body: impl FnOnce(&mut ExecutionState, &InvocationContext, &LocalContext) -> R,
    ) -> R {
        self.run_inner(version, epoch, false, setup_contract, body)
    }

    /// Same as [`Self::run`] but inside a read-only frame, for exercising
    /// read-only guards.
    pub fn run_read_only<R>(
        &mut self,
        version: ClarityVersion,
        epoch: StacksEpochId,
        setup_contract: impl FnOnce(&mut ContractContext),
        body: impl FnOnce(&mut ExecutionState, &InvocationContext, &LocalContext) -> R,
    ) -> R {
        self.run_inner(version, epoch, true, setup_contract, body)
    }

    fn run_inner<R>(
        &mut self,
        version: ClarityVersion,
        epoch: StacksEpochId,
        read_only: bool,
        setup_contract: impl FnOnce(&mut ContractContext),
        body: impl FnOnce(&mut ExecutionState, &InvocationContext, &LocalContext) -> R,
    ) -> R {
        let mut global_context = GlobalContext::new(
            false,
            CHAIN_ID_TESTNET,
            self.0.as_clarity_db(),
            LimitedCostTracker::new_free(),
            epoch,
        );
        let mut contract_context =
            ContractContext::new(QualifiedContractIdentifier::transient(), version);
        setup_contract(&mut contract_context);
        let context = LocalContext::new();
        let mut call_stack = CallStack::new();

        if read_only {
            global_context.begin_read_only();
        } else {
            global_context.begin();
        }
        let mut exec_state = ExecutionState {
            global_context: &mut global_context,
            call_stack: &mut call_stack,
        };
        let invoke_ctx = InvocationContext {
            contract_context: &contract_context,
            sender: None,
            caller: None,
            sponsor: None,
        };
        body(&mut exec_state, &invoke_ctx, &context)
    }
}
