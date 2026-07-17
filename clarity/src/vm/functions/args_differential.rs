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

//! Differential tests for the typed argument-extraction refactor
//! (`vm::functions::args`).
//!
//! `legacy_*` below are VERBATIM pre-refactor copies of the reference
//! functions and of the database writer twins (from the commit preceding the
//! conversion). Each proptest drives
//! the legacy and converted implementations with the same randomized argument
//! expressions in fresh, identical environments and asserts an identical
//! `Result` and identical emitted events. Delete the legacy copies when the
//! conversion PR merges.
//!
//! Cost-charge ORDER is not asserted here: with `MemoryBackingStore` only a
//! free tracker is available, and recording charge sequences would need
//! `GlobalContext` to be generic over `CostTracker`. Order safety rests on the
//! conversions being pure in-place line substitutions plus the precedence
//! characterization tests in each `vm::functions::*::test` module.

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::StacksEpochId;

use super::assets::{special_mint_token, special_transfer_asset_v200, special_transfer_asset_v205};
use super::database::{
    special_delete_entry_v200, special_delete_entry_v205, special_insert_entry_v200,
    special_insert_entry_v205, special_set_entry_v200, special_set_entry_v205,
    special_set_variable_v200, special_set_variable_v205,
};
use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::{CostTracker, LimitedCostTracker, MemoryConsumer, runtime_cost};
use crate::vm::database::{
    DataMapMetadata, DataVariableMetadata, FungibleTokenMetadata, MemoryBackingStore,
    NonFungibleTokenMetadata,
};
use crate::vm::errors::{
    RuntimeCheckErrorKind, RuntimeError, VmExecutionError, VmInternalError, check_argument_count,
};
use crate::vm::events::StacksTransactionEvent;
use crate::vm::types::{
    AssetIdentifier, PrincipalData, QualifiedContractIdentifier, StandardPrincipalData,
    TypeSignature,
};
use crate::vm::{
    CallStack, ClarityName, ClarityVersion, ContractContext, GlobalContext, LocalContext,
    SymbolicExpression, Value, eval,
};

type SpecialFn = fn(
    &[SymbolicExpression],
    &mut ExecutionState,
    &InvocationContext,
    &LocalContext,
) -> Result<Value, VmExecutionError>;

/// Run one special function in a fresh environment; returns the result and
/// the emitted events, both structurally comparable.
type RunOutcome = (
    Result<Value, VmExecutionError>,
    Vec<(Vec<StacksTransactionEvent>, u64)>,
);

fn run_in_fresh_env(f: SpecialFn, args: &[SymbolicExpression]) -> RunOutcome {
    run_in_fresh_env_with(f, args, false)
}

/// `read_only` runs `f` inside a read-only frame, exercising the writers'
/// leading read-only guards.
fn run_in_fresh_env_with(f: SpecialFn, args: &[SymbolicExpression], read_only: bool) -> RunOutcome {
    let mut marf = MemoryBackingStore::new();
    let mut global_context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        marf.as_clarity_db(),
        LimitedCostTracker::new_free(),
        StacksEpochId::latest(),
    );
    let mut contract_context = ContractContext::new(
        QualifiedContractIdentifier::transient(),
        ClarityVersion::latest(),
    );
    contract_context.meta_ft.insert(
        ClarityName::from_literal("stackaroo"),
        FungibleTokenMetadata { total_supply: None },
    );
    contract_context.meta_nft.insert(
        ClarityName::from_literal("stackaroo"),
        NonFungibleTokenMetadata {
            key_type: TypeSignature::UIntType,
        },
    );
    contract_context.meta_data_var.insert(
        ClarityName::from_literal("stackaroo"),
        DataVariableMetadata {
            value_type: TypeSignature::UIntType,
        },
    );
    contract_context.meta_data_map.insert(
        ClarityName::from_literal("stackaroo"),
        DataMapMetadata {
            key_type: TypeSignature::UIntType,
            value_type: TypeSignature::UIntType,
        },
    );
    let context = LocalContext::new();
    let mut call_stack = CallStack::new();

    if read_only {
        global_context.begin_read_only();
    } else {
        global_context.begin();
    }
    let result = {
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
        f(args, &mut exec_state, &invoke_ctx, &context)
    };
    let events = global_context
        .event_batches
        .iter()
        .map(|(batch, size)| (batch.events.clone(), *size))
        .collect();
    (result, events)
}

// ---------------------------------------------------------------------------
// VERBATIM legacy copies (pre-refactor), including their private error enums
// and the clarity_ecode! macro they rely on.
// ---------------------------------------------------------------------------

macro_rules! clarity_ecode {
    ($thing:expr) => {
        Ok(Value::err_uint($thing as u128))
    };
}

#[allow(non_camel_case_types)]
enum MintTokenErrorCodes {
    NON_POSITIVE_AMOUNT = 1,
}
#[allow(non_camel_case_types)]
enum TransferAssetErrorCodes {
    NOT_OWNED_BY = 1,
    SENDER_IS_RECIPIENT = 2,
    DOES_NOT_EXIST = 3,
}

fn legacy_special_mint_token(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::FtMint, exec_state, 0)?;

    let token_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Bad token name".to_string(),
        ))?;

    let amount = eval(&args[1], exec_state, invoke_ctx, context)?;
    let to = eval(&args[2], exec_state, invoke_ctx, context)?;

    if let (Value::UInt(amount), Value::Principal(to_principal)) = (amount.as_ref(), to.as_ref()) {
        if *amount == 0 {
            return clarity_ecode!(MintTokenErrorCodes::NON_POSITIVE_AMOUNT);
        }

        let ft_info = invoke_ctx.contract_context.meta_ft.get(token_name).ok_or(
            RuntimeCheckErrorKind::Unreachable(format!("No such FT: {token_name}")),
        )?;

        exec_state
            .global_context
            .database
            .checked_increase_token_supply(
                &invoke_ctx.contract_context.contract_identifier,
                token_name,
                *amount,
                ft_info,
            )?;

        let to_bal = exec_state.global_context.database.get_ft_balance(
            &invoke_ctx.contract_context.contract_identifier,
            token_name,
            to_principal,
            Some(ft_info),
        )?;

        let final_to_bal = to_bal
            .checked_add(*amount)
            .ok_or_else(|| VmInternalError::Expect("STX overflow".into()))?;

        exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
        exec_state.add_memory(TypeSignature::UIntType.size()?.into())?;

        exec_state.global_context.database.set_ft_balance(
            &invoke_ctx.contract_context.contract_identifier,
            token_name,
            to_principal,
            final_to_bal,
        )?;

        let asset_identifier = AssetIdentifier {
            contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
            asset_name: token_name.clone(),
        };
        exec_state.register_ft_mint_event(to_principal.clone(), *amount, asset_identifier)?;

        Ok(Value::okay_true())
    } else {
        Err(RuntimeCheckErrorKind::Unreachable("Bad mint FT args".to_string()).into())
    }
}

fn legacy_special_transfer_asset_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(4, args)?;

    let asset_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Bad token name".to_string(),
        ))?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let from = eval(&args[2], exec_state, invoke_ctx, context)?;
    let to = eval(&args[3], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.meta_nft.get(asset_name).ok_or(
        RuntimeCheckErrorKind::Unreachable(format!("No such NFT: {asset_name}")),
    )?;
    let expected_asset_type = &nft_metadata.key_type;

    runtime_cost(
        ClarityCostFunction::NftTransfer,
        exec_state,
        expected_asset_type.size()?,
    )?;

    if !expected_asset_type.admits(exec_state.epoch(), asset.as_ref())? {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(expected_asset_type.clone()),
            asset.as_ref().to_error_string(),
        )
        .into());
    }

    if let (Value::Principal(from_principal), Value::Principal(to_principal)) =
        (from.as_ref(), to.as_ref())
    {
        if from_principal == to_principal {
            return clarity_ecode!(TransferAssetErrorCodes::SENDER_IS_RECIPIENT);
        }

        let current_owner = match exec_state.global_context.database.get_nft_owner(
            &invoke_ctx.contract_context.contract_identifier,
            asset_name,
            asset.as_ref(),
            expected_asset_type,
        ) {
            Ok(owner) => Ok(owner),
            Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => {
                return clarity_ecode!(TransferAssetErrorCodes::DOES_NOT_EXIST);
            }
            Err(e) => Err(e),
        }?;

        if current_owner != *from_principal {
            return clarity_ecode!(TransferAssetErrorCodes::NOT_OWNED_BY);
        }

        exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
        exec_state.add_memory(expected_asset_type.size()?.into())?;

        let epoch = *exec_state.epoch();
        exec_state.global_context.database.set_nft_owner(
            &invoke_ctx.contract_context.contract_identifier,
            asset_name,
            asset.as_ref(),
            to_principal,
            expected_asset_type,
            &epoch,
        )?;

        let asset = asset.clone_with_cost(exec_state)?;
        exec_state.global_context.log_asset_transfer(
            from_principal,
            &invoke_ctx.contract_context.contract_identifier,
            asset_name,
            asset.clone(),
        )?;

        let asset_identifier = AssetIdentifier {
            contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
            asset_name: asset_name.clone(),
        };
        exec_state.register_nft_transfer_event(
            from_principal.clone(),
            to_principal.clone(),
            asset,
            asset_identifier,
        )?;

        Ok(Value::okay_true())
    } else {
        Err(RuntimeCheckErrorKind::Unreachable("Bad transfer NFT args".to_string()).into())
    }
}

/// The Stacks v205 version of transfer_asset uses the actual stored size of the
///  asset as input to the cost tabulation. Otherwise identical to v200.
fn legacy_special_transfer_asset_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(4, args)?;

    let asset_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Bad token name".to_string(),
        ))?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let from = eval(&args[2], exec_state, invoke_ctx, context)?;
    let to = eval(&args[3], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.meta_nft.get(asset_name).ok_or(
        RuntimeCheckErrorKind::Unreachable(format!("No such NFT: {asset_name}")),
    )?;
    let expected_asset_type = &nft_metadata.key_type;

    let asset_size = asset
        .as_ref()
        .serialized_size()
        .map_err(|e| VmInternalError::Expect(e.to_string()))? as u64;
    runtime_cost(ClarityCostFunction::NftTransfer, exec_state, asset_size)?;

    if !expected_asset_type.admits(exec_state.epoch(), asset.as_ref())? {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(expected_asset_type.clone()),
            asset.as_ref().to_error_string(),
        )
        .into());
    }

    if let (Value::Principal(from_principal), Value::Principal(to_principal)) =
        (from.as_ref(), to.as_ref())
    {
        if from_principal == to_principal {
            return clarity_ecode!(TransferAssetErrorCodes::SENDER_IS_RECIPIENT);
        }

        let current_owner = match exec_state.global_context.database.get_nft_owner(
            &invoke_ctx.contract_context.contract_identifier,
            asset_name,
            asset.as_ref(),
            expected_asset_type,
        ) {
            Ok(owner) => Ok(owner),
            Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => {
                return clarity_ecode!(TransferAssetErrorCodes::DOES_NOT_EXIST);
            }
            Err(e) => Err(e),
        }?;

        if current_owner != *from_principal {
            return clarity_ecode!(TransferAssetErrorCodes::NOT_OWNED_BY);
        }

        exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
        exec_state.add_memory(asset_size)?;

        let epoch = *exec_state.epoch();
        exec_state.global_context.database.set_nft_owner(
            &invoke_ctx.contract_context.contract_identifier,
            asset_name,
            asset.as_ref(),
            to_principal,
            expected_asset_type,
            &epoch,
        )?;

        let asset = asset.clone_with_cost(exec_state)?;
        exec_state.global_context.log_asset_transfer(
            from_principal,
            &invoke_ctx.contract_context.contract_identifier,
            asset_name,
            asset.clone(),
        )?;

        let asset_identifier = AssetIdentifier {
            contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
            asset_name: asset_name.clone(),
        };
        exec_state.register_nft_transfer_event(
            from_principal.clone(),
            to_principal.clone(),
            asset,
            asset_identifier,
        )?;

        Ok(Value::okay_true())
    } else {
        Err(RuntimeCheckErrorKind::Unreachable("Bad transfer NFT args".to_string()).into())
    }
}

// ---------------------------------------------------------------------------
// Strategies and differential proptests
// ---------------------------------------------------------------------------

fn arb_name_expr() -> impl Strategy<Value = SymbolicExpression> {
    prop_oneof![
        Just(SymbolicExpression::atom(ClarityName::from_literal(
            "stackaroo"
        ))),
        Just(SymbolicExpression::atom(ClarityName::from_literal(
            "rocket"
        ))),
        Just(SymbolicExpression::atom_value(Value::UInt(1))),
    ]
}

fn arb_value_expr() -> impl Strategy<Value = SymbolicExpression> {
    prop_oneof![
        (0u128..4).prop_map(|n| SymbolicExpression::atom_value(Value::UInt(n))),
        Just(SymbolicExpression::atom_value(Value::UInt(u128::MAX))),
        any::<i128>().prop_map(|n| SymbolicExpression::atom_value(Value::Int(n))),
        any::<bool>().prop_map(|b| SymbolicExpression::atom_value(Value::Bool(b))),
        Just(SymbolicExpression::atom_value(Value::Principal(
            PrincipalData::Standard(StandardPrincipalData::transient()),
        ))),
        Just(SymbolicExpression::atom_value(
            Value::buff_from(vec![7]).unwrap(),
        )),
    ]
}

fn assert_equivalent(
    legacy: SpecialFn,
    converted: SpecialFn,
    args: &[SymbolicExpression],
) -> Result<(), TestCaseError> {
    let (legacy_result, legacy_events) = run_in_fresh_env(legacy, args);
    let (converted_result, converted_events) = run_in_fresh_env(converted, args);
    prop_assert_eq!(legacy_result, converted_result, "diverging Result");
    prop_assert_eq!(legacy_events, converted_events, "diverging events");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mint_token_differential(
        name in arb_name_expr(),
        amount in arb_value_expr(),
        to in arb_value_expr(),
    ) {
        let args = [name, amount, to];
        assert_equivalent(legacy_special_mint_token, special_mint_token, &args)?;
    }

    #[test]
    fn transfer_asset_v200_differential(
        name in arb_name_expr(),
        asset in arb_value_expr(),
        from in arb_value_expr(),
        to in arb_value_expr(),
    ) {
        let args = [name, asset, from, to];
        assert_equivalent(
            legacy_special_transfer_asset_v200,
            special_transfer_asset_v200,
            &args,
        )?;
    }

    #[test]
    fn transfer_asset_v205_differential(
        name in arb_name_expr(),
        asset in arb_value_expr(),
        from in arb_value_expr(),
        to in arb_value_expr(),
    ) {
        let args = [name, asset, from, to];
        assert_equivalent(
            legacy_special_transfer_asset_v205,
            special_transfer_asset_v205,
            &args,
        )?;
    }
}

// ---------------------------------------------------------------------------
// VERBATIM legacy copies of the database writer twins (pre-refactor).
// ---------------------------------------------------------------------------

fn legacy_special_set_variable_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(2, args)?;

    let value = eval(&args[1], exec_state, invoke_ctx, context)?;

    let var_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_var
        .get(var_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such data variable: {var_name}"
        )))?;

    runtime_cost(
        ClarityCostFunction::SetVar,
        exec_state,
        data_types.value_type.size()?,
    )?;

    exec_state.add_memory(value.as_ref().get_memory_use()?)?;

    let value = value.clone_with_cost(exec_state)?;
    let epoch = *exec_state.epoch();
    exec_state
        .global_context
        .database
        .set_variable(contract, var_name, value, data_types, &epoch)
        .map(|data| data.value)
}

fn legacy_special_set_variable_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(2, args)?;

    let value = eval(&args[1], exec_state, invoke_ctx, context)?;

    let var_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_var
        .get(var_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such data variable: {var_name}"
        )))?;

    let value = value.clone_with_cost(exec_state)?;
    let epoch = *exec_state.epoch();
    let result = exec_state
        .global_context
        .database
        .set_variable(contract, var_name, value, data_types, &epoch);

    let result_size = match &result {
        Ok(data) => data.serialized_byte_len,
        Err(_e) => data_types.value_type.size()?.into(),
    };

    runtime_cost(ClarityCostFunction::SetVar, exec_state, result_size)?;

    exec_state.add_memory(result_size)?;

    result.map(|data| data.value)
}

fn legacy_special_set_entry_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(3, args)?;

    let key = eval(&args[1], exec_state, invoke_ctx, context)?;

    let value = eval(&args[2], exec_state, invoke_ctx, context)?;

    let map_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_map
        .get(map_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such map: {map_name}"
        )))?;

    runtime_cost(
        ClarityCostFunction::SetEntry,
        exec_state,
        data_types.value_type.size()? + data_types.key_type.size()?,
    )?;

    exec_state.add_memory(key.as_ref().get_memory_use()?)?;
    exec_state.add_memory(value.as_ref().get_memory_use()?)?;

    let key = key.clone_with_cost(exec_state)?;
    let value = value.clone_with_cost(exec_state)?;
    let epoch = *exec_state.epoch();
    exec_state
        .global_context
        .database
        .set_entry(contract, map_name, key, value, data_types, &epoch)
        .map(|data| data.value)
}

fn legacy_special_set_entry_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(3, args)?;

    let key = eval(&args[1], exec_state, invoke_ctx, context)?;

    let value = eval(&args[2], exec_state, invoke_ctx, context)?;

    let map_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_map
        .get(map_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such map: {map_name}"
        )))?;

    let key = key.clone_with_cost(exec_state)?;
    let value = value.clone_with_cost(exec_state)?;
    let epoch = *exec_state.epoch();
    let result = exec_state
        .global_context
        .database
        .set_entry(contract, map_name, key, value, data_types, &epoch);

    let result_size = match &result {
        Ok(data) => data.serialized_byte_len,
        Err(_e) => (data_types.value_type.size()? + data_types.key_type.size()?).into(),
    };

    runtime_cost(ClarityCostFunction::SetEntry, exec_state, result_size)?;

    exec_state.add_memory(result_size)?;

    result.map(|data| data.value)
}

fn legacy_special_insert_entry_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(3, args)?;

    let key = eval(&args[1], exec_state, invoke_ctx, context)?;

    let value = eval(&args[2], exec_state, invoke_ctx, context)?;

    let map_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_map
        .get(map_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such map: {map_name}"
        )))?;

    runtime_cost(
        ClarityCostFunction::SetEntry,
        exec_state,
        data_types.value_type.size()? + data_types.key_type.size()?,
    )?;

    exec_state.add_memory(key.as_ref().get_memory_use()?)?;
    exec_state.add_memory(value.as_ref().get_memory_use()?)?;

    let epoch = *exec_state.epoch();

    let key = key.clone_with_cost(exec_state)?;
    let value = value.clone_with_cost(exec_state)?;
    exec_state
        .global_context
        .database
        .insert_entry(contract, map_name, key, value, data_types, &epoch)
        .map(|data| data.value)
}

fn legacy_special_insert_entry_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(3, args)?;

    let key = eval(&args[1], exec_state, invoke_ctx, context)?;

    let value = eval(&args[2], exec_state, invoke_ctx, context)?;

    let map_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_map
        .get(map_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such map: {map_name}"
        )))?;

    let key = key.clone_with_cost(exec_state)?;
    let value = value.clone_with_cost(exec_state)?;
    let epoch = *exec_state.epoch();
    let result = exec_state
        .global_context
        .database
        .insert_entry(contract, map_name, key, value, data_types, &epoch);

    let result_size = match &result {
        Ok(data) => data.serialized_byte_len,
        Err(_e) => (data_types.value_type.size()? + data_types.key_type.size()?).into(),
    };

    runtime_cost(ClarityCostFunction::SetEntry, exec_state, result_size)?;

    exec_state.add_memory(result_size)?;

    result.map(|data| data.value)
}

fn legacy_special_delete_entry_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(2, args)?;

    let key = eval(&args[1], exec_state, invoke_ctx, context)?;

    let map_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_map
        .get(map_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such map: {map_name}"
        )))?;

    runtime_cost(
        ClarityCostFunction::SetEntry,
        exec_state,
        data_types.key_type.size()?,
    )?;

    exec_state.add_memory(key.as_ref().get_memory_use()?)?;

    let epoch = *exec_state.epoch();
    exec_state
        .global_context
        .database
        .delete_entry(contract, map_name, key.as_ref(), data_types, &epoch)
        .map(|data| data.value)
}

fn legacy_special_delete_entry_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        return Err(
            RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into(),
        );
    }

    check_argument_count(2, args)?;

    let key = eval(&args[1], exec_state, invoke_ctx, context)?;

    let map_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable(
            "Expected name".to_string(),
        ))?;

    let contract = &invoke_ctx.contract_context.contract_identifier;

    let data_types = invoke_ctx
        .contract_context
        .meta_data_map
        .get(map_name)
        .ok_or(RuntimeCheckErrorKind::Unreachable(format!(
            "No such map: {map_name}"
        )))?;

    let epoch = *exec_state.epoch();
    let result = exec_state.global_context.database.delete_entry(
        contract,
        map_name,
        key.as_ref(),
        data_types,
        &epoch,
    );

    let result_size = match &result {
        Ok(data) => data.serialized_byte_len,
        Err(_e) => data_types.key_type.size()?.into(),
    };

    runtime_cost(ClarityCostFunction::SetEntry, exec_state, result_size)?;

    exec_state.add_memory(result_size)?;

    result.map(|data| data.value)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn set_variable_v200_differential(
        name in arb_name_expr(),
        value in arb_value_expr(),
    ) {
        let args = [name, value];
        assert_equivalent(legacy_special_set_variable_v200, special_set_variable_v200, &args)?;
    }

    #[test]
    fn set_variable_v205_differential(
        name in arb_name_expr(),
        value in arb_value_expr(),
    ) {
        let args = [name, value];
        assert_equivalent(legacy_special_set_variable_v205, special_set_variable_v205, &args)?;
    }

    #[test]
    fn set_entry_v200_differential(
        name in arb_name_expr(),
        key in arb_value_expr(),
        value in arb_value_expr(),
    ) {
        let args = [name, key, value];
        assert_equivalent(legacy_special_set_entry_v200, special_set_entry_v200, &args)?;
    }

    #[test]
    fn set_entry_v205_differential(
        name in arb_name_expr(),
        key in arb_value_expr(),
        value in arb_value_expr(),
    ) {
        let args = [name, key, value];
        assert_equivalent(legacy_special_set_entry_v205, special_set_entry_v205, &args)?;
    }

    #[test]
    fn insert_entry_v200_differential(
        name in arb_name_expr(),
        key in arb_value_expr(),
        value in arb_value_expr(),
    ) {
        let args = [name, key, value];
        assert_equivalent(legacy_special_insert_entry_v200, special_insert_entry_v200, &args)?;
    }

    #[test]
    fn insert_entry_v205_differential(
        name in arb_name_expr(),
        key in arb_value_expr(),
        value in arb_value_expr(),
    ) {
        let args = [name, key, value];
        assert_equivalent(legacy_special_insert_entry_v205, special_insert_entry_v205, &args)?;
    }

    #[test]
    fn delete_entry_v200_differential(
        name in arb_name_expr(),
        key in arb_value_expr(),
    ) {
        let args = [name, key];
        assert_equivalent(legacy_special_delete_entry_v200, special_delete_entry_v200, &args)?;
    }

    #[test]
    fn delete_entry_v205_differential(
        name in arb_name_expr(),
        key in arb_value_expr(),
    ) {
        let args = [name, key];
        assert_equivalent(legacy_special_delete_entry_v205, special_delete_entry_v205, &args)?;
    }
}

/// The writers' read-only guard is their first statement; equivalence under a
/// read-only frame is deterministic, so no proptest needed.
#[test]
fn writer_read_only_guard_differential() {
    let name = || SymbolicExpression::atom(ClarityName::from_literal("stackaroo"));
    let uint = |n: u128| SymbolicExpression::atom_value(Value::UInt(n));

    let cases: [(SpecialFn, SpecialFn, Vec<SymbolicExpression>); 8] = [
        (
            legacy_special_set_variable_v200,
            special_set_variable_v200,
            vec![name(), uint(2)],
        ),
        (
            legacy_special_set_variable_v205,
            special_set_variable_v205,
            vec![name(), uint(2)],
        ),
        (
            legacy_special_set_entry_v200,
            special_set_entry_v200,
            vec![name(), uint(1), uint(2)],
        ),
        (
            legacy_special_set_entry_v205,
            special_set_entry_v205,
            vec![name(), uint(1), uint(2)],
        ),
        (
            legacy_special_insert_entry_v200,
            special_insert_entry_v200,
            vec![name(), uint(1), uint(2)],
        ),
        (
            legacy_special_insert_entry_v205,
            special_insert_entry_v205,
            vec![name(), uint(1), uint(2)],
        ),
        (
            legacy_special_delete_entry_v200,
            special_delete_entry_v200,
            vec![name(), uint(1)],
        ),
        (
            legacy_special_delete_entry_v205,
            special_delete_entry_v205,
            vec![name(), uint(1)],
        ),
    ];
    for (legacy, converted, args) in cases {
        let legacy_outcome = run_in_fresh_env_with(legacy, &args, true);
        let converted_outcome = run_in_fresh_env_with(converted, &args, true);
        assert_eq!(legacy_outcome, converted_outcome);
    }
}
