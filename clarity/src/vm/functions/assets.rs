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

use stacks_common::types::StacksEpochId;

use super::args::{
    ContractContextExt, ensure_admits, eval_args, expect_principal_value, name_atom, typed_refs,
};
use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::{CostTracker, runtime_cost};
use crate::vm::database::STXBalance;
use crate::vm::errors::{
    RuntimeCheckErrorKind, RuntimeError, VmExecutionError, VmInternalError, check_argument_count,
};
use crate::vm::representations::SymbolicExpression;
use crate::vm::types::{AssetIdentifier, BuffData, PrincipalData, TupleData, TypeSignature, Value};
use crate::vm::{LocalContext, eval};

enum MintAssetErrorCodes {
    ALREADY_EXIST = 1,
}
enum MintTokenErrorCodes {
    NON_POSITIVE_AMOUNT = 1,
}
enum TransferAssetErrorCodes {
    NOT_OWNED_BY = 1,
    SENDER_IS_RECIPIENT = 2,
    DOES_NOT_EXIST = 3,
}
enum TransferTokenErrorCodes {
    NOT_ENOUGH_BALANCE = 1,
    SENDER_IS_RECIPIENT = 2,
    NON_POSITIVE_AMOUNT = 3,
}

enum BurnAssetErrorCodes {
    NOT_OWNED_BY = 1,
    DOES_NOT_EXIST = 3,
}
enum BurnTokenErrorCodes {
    NOT_ENOUGH_BALANCE_OR_NON_POSITIVE = 1,
}

enum StxErrorCodes {
    NOT_ENOUGH_BALANCE = 1,
    SENDER_IS_RECIPIENT = 2,
    NON_POSITIVE_AMOUNT = 3,
    SENDER_IS_NOT_TX_SENDER = 4,
}

macro_rules! clarity_ecode {
    ($thing:expr) => {
        Ok(Value::err_uint($thing as u128))
    };
}

switch_on_global_epoch!(special_mint_asset(
    special_mint_asset_v200,
    special_mint_asset_v205
));

switch_on_global_epoch!(special_transfer_asset(
    special_transfer_asset_v200,
    special_transfer_asset_v205
));

switch_on_global_epoch!(special_get_owner(
    special_get_owner_v200,
    special_get_owner_v205
));

switch_on_global_epoch!(special_burn_asset(
    special_burn_asset_v200,
    special_burn_asset_v205
));

pub fn special_stx_balance(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;

    runtime_cost(ClarityCostFunction::StxBalance, exec_state, 0)?;

    let owner = eval(&args[0], exec_state, invoke_ctx, context)?;

    let principal = expect_principal_value(owner.as_ref())?;
    let balance = {
        let mut snapshot = exec_state
            .global_context
            .database
            .get_stx_balance_snapshot(principal)?;
        snapshot.get_available_balance()?
    };
    Ok(Value::UInt(balance))
}

/// Do a "consolidated" STX transfer.
/// If the 'from' principal has locked STX, and they have unlocked, then process the STX unlock
/// and update its balance in addition to spending tokens out of it.
pub fn stx_transfer_consolidated(
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    from: &PrincipalData,
    to: &PrincipalData,
    amount: u128,
    memo: &BuffData,
) -> Result<Value, VmExecutionError> {
    if amount == 0 {
        return clarity_ecode!(StxErrorCodes::NON_POSITIVE_AMOUNT);
    }

    if from == to {
        return clarity_ecode!(StxErrorCodes::SENDER_IS_RECIPIENT);
    }

    if Some(from) != invoke_ctx.sender.as_ref() {
        return clarity_ecode!(StxErrorCodes::SENDER_IS_NOT_TX_SENDER);
    }

    // loading from/to principals and balances
    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    // loading from's locked amount and height
    // TODO: this does not count the inner stacks block header load, but arguably,
    // this could be optimized away, so it shouldn't penalize the caller.
    exec_state.add_memory(STXBalance::unlocked_and_v1_size as u64)?;
    exec_state.add_memory(STXBalance::unlocked_and_v1_size as u64)?;

    let mut sender_snapshot = exec_state
        .global_context
        .database
        .get_stx_balance_snapshot(from)?;
    if !sender_snapshot.can_transfer(amount)? {
        return clarity_ecode!(StxErrorCodes::NOT_ENOUGH_BALANCE);
    }

    sender_snapshot.transfer_to(to, amount)?;

    exec_state.global_context.log_stx_transfer(from, amount)?;
    exec_state.register_stx_transfer_event(from.clone(), to.clone(), amount, memo.clone())?;
    Ok(Value::okay_true())
}

pub fn special_stx_transfer(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::StxTransfer, exec_state, 0)?;

    let vals = eval_args::<3>(args, exec_state, invoke_ctx, context)?;
    let (amount, from, to): (u128, &PrincipalData, &PrincipalData) = typed_refs(&vals, || {
        RuntimeCheckErrorKind::Unreachable("Bad transfer STX args".to_string())
    })?;
    let memo = BuffData::empty();

    stx_transfer_consolidated(exec_state, invoke_ctx, from, to, amount, &memo)
}

pub fn special_stx_transfer_memo(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(4, args)?;
    runtime_cost(ClarityCostFunction::StxTransferMemo, exec_state, 0)?;

    let vals = eval_args::<4>(args, exec_state, invoke_ctx, context)?;
    let (amount, from, to, memo): (u128, &PrincipalData, &PrincipalData, &BuffData) =
        typed_refs(&vals, || {
            RuntimeCheckErrorKind::Unreachable("Bad transfer STX args".to_string())
        })?;

    stx_transfer_consolidated(exec_state, invoke_ctx, from, to, amount, memo)
}

#[allow(clippy::unnecessary_fallible_conversions)]
pub fn special_stx_account(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;

    runtime_cost(ClarityCostFunction::StxGetAccount, exec_state, 0)?;

    let owner = eval(&args[0], exec_state, invoke_ctx, context)?;
    let principal = expect_principal_value(owner.as_ref())?;

    let stx_balance = exec_state
        .global_context
        .database
        .get_stx_balance_snapshot(principal)?
        .canonical_balance_repr()?;
    let v1_unlock_ht = exec_state.global_context.database.get_v1_unlock_height();
    let v2_unlock_ht = exec_state.global_context.database.get_v2_unlock_height()?;
    let v3_unlock_ht = exec_state.global_context.database.get_v3_unlock_height()?;
    let v4_unlock_ht = exec_state.global_context.database.get_v4_unlock_height()?;

    Ok(TupleData::from_data(vec![
        (
            "unlocked"
                .try_into()
                .map_err(|_| VmInternalError::Expect("Bad special tuple name".into()))?,
            Value::UInt(stx_balance.amount_unlocked()),
        ),
        (
            "locked"
                .try_into()
                .map_err(|_| VmInternalError::Expect("Bad special tuple name".into()))?,
            Value::UInt(stx_balance.amount_locked()),
        ),
        (
            "unlock-height"
                .try_into()
                .map_err(|_| VmInternalError::Expect("Bad special tuple name".into()))?,
            Value::UInt(u128::from(stx_balance.effective_unlock_height(
                v1_unlock_ht,
                v2_unlock_ht,
                v3_unlock_ht,
                v4_unlock_ht,
            ))),
        ),
    ])
    .map(Value::Tuple)?)
}

pub fn special_stx_burn(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    runtime_cost(ClarityCostFunction::StxTransfer, exec_state, 0)?;

    let vals = eval_args::<2>(args, exec_state, invoke_ctx, context)?;
    let (amount, from): (u128, &PrincipalData) = typed_refs(&vals, || {
        RuntimeCheckErrorKind::Unreachable("Bad transfer STX args".to_string())
    })?;
    if amount == 0 {
        return clarity_ecode!(StxErrorCodes::NON_POSITIVE_AMOUNT);
    }

    if Some(from) != invoke_ctx.sender.as_ref() {
        return clarity_ecode!(StxErrorCodes::SENDER_IS_NOT_TX_SENDER);
    }

    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    exec_state.add_memory(STXBalance::unlocked_and_v1_size.try_into().map_err(|_| {
        RuntimeCheckErrorKind::Unreachable(
            "BUG: STXBalance::unlocked_and_v1_size does not fit into a u64".into(),
        )
    })?)?;

    let mut burner_snapshot = exec_state
        .global_context
        .database
        .get_stx_balance_snapshot(from)?;
    if !burner_snapshot.can_transfer(amount)? {
        return clarity_ecode!(StxErrorCodes::NOT_ENOUGH_BALANCE);
    }

    burner_snapshot.debit(amount)?;
    burner_snapshot.save()?;

    exec_state
        .global_context
        .database
        .decrement_ustx_liquid_supply(amount)?;

    exec_state.global_context.log_stx_burn(from, amount)?;
    exec_state.register_stx_burn_event(from.clone(), amount)?;

    Ok(Value::okay_true())
}

pub fn special_mint_token(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::FtMint, exec_state, 0)?;

    let token_name = name_atom(&args[0], "Bad token name")?;

    let vals = eval_args::<2>(&args[1..], exec_state, invoke_ctx, context)?;
    let (amount, to_principal): (u128, &PrincipalData) = typed_refs(&vals, || {
        RuntimeCheckErrorKind::Unreachable("Bad mint FT args".to_string())
    })?;

    if amount == 0 {
        return clarity_ecode!(MintTokenErrorCodes::NON_POSITIVE_AMOUNT);
    }

    let ft_info = invoke_ctx.contract_context.ft_info_checked(token_name)?;

    exec_state
        .global_context
        .database
        .checked_increase_token_supply(
            &invoke_ctx.contract_context.contract_identifier,
            token_name,
            amount,
            ft_info,
        )?;

    let to_bal = exec_state.global_context.database.get_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        to_principal,
        Some(ft_info),
    )?;

    let final_to_bal = to_bal
        .checked_add(amount)
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
    exec_state.register_ft_mint_event(to_principal.clone(), amount, asset_identifier)?;

    Ok(Value::okay_true())
}

pub fn special_mint_asset_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let to = eval(&args[2], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    runtime_cost(
        ClarityCostFunction::NftMint,
        exec_state,
        expected_asset_type.size()?,
    )?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    let to_principal = expect_principal_value(to.as_ref())?;
    match exec_state.global_context.database.get_nft_owner(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
    ) {
        Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => Ok(()),
        Ok(_owner) => return clarity_ecode!(MintAssetErrorCodes::ALREADY_EXIST),
        Err(e) => Err(e),
    }?;

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

    let asset_identifier = AssetIdentifier {
        contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
        asset_name: asset_name.clone(),
    };
    let asset = asset.clone_with_cost(exec_state)?;
    exec_state.register_nft_mint_event(to_principal.clone(), asset, asset_identifier)?;

    Ok(Value::okay_true())
}

/// The Stacks v205 version of mint_asset uses the actual stored size of the
///  asset as input to the cost tabulation. Otherwise identical to v200.
pub fn special_mint_asset_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let to = eval(&args[2], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    let asset_size = asset
        .as_ref()
        .serialized_size()
        .map_err(|e| VmInternalError::Expect(e.to_string()))? as u64;
    runtime_cost(ClarityCostFunction::NftMint, exec_state, asset_size)?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    let to_principal = expect_principal_value(to.as_ref())?;
    match exec_state.global_context.database.get_nft_owner(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
    ) {
        Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => Ok(()),
        Ok(_owner) => return clarity_ecode!(MintAssetErrorCodes::ALREADY_EXIST),
        Err(e) => Err(e),
    }?;

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

    let asset_identifier = AssetIdentifier {
        contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
        asset_name: asset_name.clone(),
    };
    let asset = asset.clone_with_cost(exec_state)?;
    exec_state.register_nft_mint_event(to_principal.clone(), asset, asset_identifier)?;

    Ok(Value::okay_true())
}

pub fn special_transfer_asset_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(4, args)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    // The asset stays a raw ValueRef: its clone_with_cost below is a
    // consensus-visible cost position.
    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let from_to_vals = eval_args::<2>(&args[2..], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    runtime_cost(
        ClarityCostFunction::NftTransfer,
        exec_state,
        expected_asset_type.size()?,
    )?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    let (from_principal, to_principal): (&PrincipalData, &PrincipalData) =
        typed_refs(&from_to_vals, || {
            RuntimeCheckErrorKind::Unreachable("Bad transfer NFT args".to_string())
        })?;

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
}

/// The Stacks v205 version of transfer_asset uses the actual stored size of the
///  asset as input to the cost tabulation. Otherwise identical to v200.
pub fn special_transfer_asset_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(4, args)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    // The asset stays a raw ValueRef: its clone_with_cost below is a
    // consensus-visible cost position.
    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let from_to_vals = eval_args::<2>(&args[2..], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    let asset_size = asset
        .as_ref()
        .serialized_size()
        .map_err(|e| VmInternalError::Expect(e.to_string()))? as u64;
    runtime_cost(ClarityCostFunction::NftTransfer, exec_state, asset_size)?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    let (from_principal, to_principal): (&PrincipalData, &PrincipalData) =
        typed_refs(&from_to_vals, || {
            RuntimeCheckErrorKind::Unreachable("Bad transfer NFT args".to_string())
        })?;

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
}

pub fn special_transfer_token(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(4, args)?;

    runtime_cost(ClarityCostFunction::FtTransfer, exec_state, 0)?;

    let token_name = name_atom(&args[0], "Bad token name")?;

    let vals = eval_args::<3>(&args[1..], exec_state, invoke_ctx, context)?;
    let (amount, from_principal, to_principal): (u128, &PrincipalData, &PrincipalData) =
        typed_refs(&vals, || {
            RuntimeCheckErrorKind::Unreachable("Bad transfer FT args".to_string())
        })?;
    if amount == 0 {
        return clarity_ecode!(TransferTokenErrorCodes::NON_POSITIVE_AMOUNT);
    }

    if from_principal == to_principal {
        return clarity_ecode!(TransferTokenErrorCodes::SENDER_IS_RECIPIENT);
    }

    let ft_info = invoke_ctx.contract_context.ft_info_checked(token_name)?;

    let from_bal = exec_state.global_context.database.get_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        from_principal,
        Some(ft_info),
    )?;

    if from_bal < amount {
        return clarity_ecode!(TransferTokenErrorCodes::NOT_ENOUGH_BALANCE);
    }

    let final_from_bal = from_bal - amount;

    let to_bal = exec_state.global_context.database.get_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        to_principal,
        Some(ft_info),
    )?;

    // `ArithmeticOverflow` in this function is **unreachable** in normal Clarity execution because:
    // - the total liquid ustx supply will overflow before such an overflowing transfer is allowed.
    let final_to_bal = to_bal
        .checked_add(amount)
        .ok_or(RuntimeError::ArithmeticOverflow)?;

    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    exec_state.add_memory(TypeSignature::UIntType.size()?.into())?;
    exec_state.add_memory(TypeSignature::UIntType.size()?.into())?;

    exec_state.global_context.database.set_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        from_principal,
        final_from_bal,
    )?;
    exec_state.global_context.database.set_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        to_principal,
        final_to_bal,
    )?;

    exec_state.global_context.log_token_transfer(
        from_principal,
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        amount,
    )?;

    let asset_identifier = AssetIdentifier {
        contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
        asset_name: token_name.clone(),
    };
    exec_state.register_ft_transfer_event(
        from_principal.clone(),
        to_principal.clone(),
        amount,
        asset_identifier,
    )?;

    Ok(Value::okay_true())
}

pub fn special_get_balance(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    runtime_cost(ClarityCostFunction::FtBalance, exec_state, 0)?;

    let token_name = name_atom(&args[0], "Bad token name")?;

    let owner = eval(&args[1], exec_state, invoke_ctx, context)?;

    let principal = expect_principal_value(owner.as_ref())?;
    let ft_info = invoke_ctx.contract_context.ft_info_checked(token_name)?;

    let balance = exec_state.global_context.database.get_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        principal,
        Some(ft_info),
    )?;
    Ok(Value::UInt(balance))
}

pub fn special_get_owner_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    runtime_cost(
        ClarityCostFunction::NftOwner,
        exec_state,
        expected_asset_type.size()?,
    )?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    match exec_state.global_context.database.get_nft_owner(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
    ) {
        Ok(owner) => Ok(Value::some(Value::Principal(owner)).map_err(|_| {
            VmInternalError::Expect("Principal should always fit in optional.".into())
        })?),
        Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => Ok(Value::none()),
        Err(e) => Err(e),
    }
}

/// The Stacks v205 version of get_owner uses the actual stored size of the
///  asset as input to the cost tabulation. Otherwise identical to v200.
pub fn special_get_owner_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    let asset_size = asset
        .as_ref()
        .serialized_size()
        .map_err(|e| VmInternalError::Expect(e.to_string()))? as u64;
    runtime_cost(ClarityCostFunction::NftOwner, exec_state, asset_size)?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    match exec_state.global_context.database.get_nft_owner(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
    ) {
        Ok(owner) => Ok(Value::some(Value::Principal(owner)).map_err(|_| {
            VmInternalError::Expect("Principal should always fit in optional.".into())
        })?),
        Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => Ok(Value::none()),
        Err(e) => Err(e),
    }
}

pub fn special_get_token_supply(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    _context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;

    runtime_cost(ClarityCostFunction::FtSupply, exec_state, 0)?;

    let token_name = name_atom(&args[0], "Bad token name")?;

    let supply = exec_state
        .global_context
        .database
        .get_ft_supply(&invoke_ctx.contract_context.contract_identifier, token_name)?;
    Ok(Value::UInt(supply))
}

pub fn special_burn_token(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::FtBurn, exec_state, 0)?;

    let token_name = name_atom(&args[0], "Bad token name")?;

    let vals = eval_args::<2>(&args[1..], exec_state, invoke_ctx, context)?;
    let (amount, burner): (u128, &PrincipalData) = typed_refs(&vals, || {
        RuntimeCheckErrorKind::Unreachable("Bad burn FT args".to_string())
    })?;
    if amount == 0 {
        return clarity_ecode!(BurnTokenErrorCodes::NOT_ENOUGH_BALANCE_OR_NON_POSITIVE);
    }

    let burner_bal = exec_state.global_context.database.get_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        burner,
        None,
    )?;

    if amount > burner_bal {
        return clarity_ecode!(BurnTokenErrorCodes::NOT_ENOUGH_BALANCE_OR_NON_POSITIVE);
    }

    exec_state
        .global_context
        .database
        .checked_decrease_token_supply(
            &invoke_ctx.contract_context.contract_identifier,
            token_name,
            amount,
        )?;

    let final_burner_bal = burner_bal - amount;

    exec_state.global_context.database.set_ft_balance(
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        burner,
        final_burner_bal,
    )?;

    let asset_identifier = AssetIdentifier {
        contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
        asset_name: token_name.clone(),
    };
    exec_state.register_ft_burn_event(burner.clone(), amount, asset_identifier)?;

    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    exec_state.add_memory(TypeSignature::UIntType.size()?.into())?;

    exec_state.global_context.log_token_transfer(
        burner,
        &invoke_ctx.contract_context.contract_identifier,
        token_name,
        amount,
    )?;

    Ok(Value::okay_true())
}

pub fn special_burn_asset_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::NftBurn, exec_state, 0)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let sender = eval(&args[2], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    runtime_cost(
        ClarityCostFunction::NftBurn,
        exec_state,
        expected_asset_type.size()?,
    )?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    let sender_principal = expect_principal_value(sender.as_ref())?;
    let owner = match exec_state.global_context.database.get_nft_owner(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
    ) {
        Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => {
            return clarity_ecode!(BurnAssetErrorCodes::DOES_NOT_EXIST);
        }
        Ok(owner) => Ok(owner),
        Err(e) => Err(e),
    }?;

    if &owner != sender_principal {
        return clarity_ecode!(BurnAssetErrorCodes::NOT_OWNED_BY);
    }

    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    exec_state.add_memory(expected_asset_type.size()?.into())?;

    let epoch = *exec_state.epoch();
    exec_state.global_context.database.burn_nft(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
        &epoch,
    )?;

    let asset = asset.clone_with_cost(exec_state)?;
    exec_state.global_context.log_asset_transfer(
        sender_principal,
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.clone(),
    )?;

    let asset_identifier = AssetIdentifier {
        contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
        asset_name: asset_name.clone(),
    };
    exec_state.register_nft_burn_event(sender_principal.clone(), asset, asset_identifier)?;

    Ok(Value::okay_true())
}

/// The Stacks v205 version of burn_asset uses the actual stored size of the
///  asset as input to the cost tabulation. Otherwise identical to v200.
pub fn special_burn_asset_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::NftBurn, exec_state, 0)?;

    let asset_name = name_atom(&args[0], "Bad token name")?;

    let asset = eval(&args[1], exec_state, invoke_ctx, context)?;
    let sender = eval(&args[2], exec_state, invoke_ctx, context)?;

    let nft_metadata = invoke_ctx.contract_context.nft_info_checked(asset_name)?;
    let expected_asset_type = &nft_metadata.key_type;

    let asset_size = asset
        .as_ref()
        .serialized_size()
        .map_err(|e| VmInternalError::Expect(e.to_string()))? as u64;
    runtime_cost(ClarityCostFunction::NftBurn, exec_state, asset_size)?;

    ensure_admits(exec_state, expected_asset_type, asset.as_ref())?;

    let sender_principal = expect_principal_value(sender.as_ref())?;
    let owner = match exec_state.global_context.database.get_nft_owner(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
    ) {
        Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => {
            return clarity_ecode!(BurnAssetErrorCodes::DOES_NOT_EXIST);
        }
        Ok(owner) => Ok(owner),
        Err(e) => Err(e),
    }?;

    if &owner != sender_principal {
        return clarity_ecode!(BurnAssetErrorCodes::NOT_OWNED_BY);
    }

    exec_state.add_memory(TypeSignature::PrincipalType.size()?.into())?;
    exec_state.add_memory(asset_size)?;

    let epoch = *exec_state.epoch();
    exec_state.global_context.database.burn_nft(
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.as_ref(),
        expected_asset_type,
        &epoch,
    )?;

    let asset = asset.clone_with_cost(exec_state)?;
    exec_state.global_context.log_asset_transfer(
        sender_principal,
        &invoke_ctx.contract_context.contract_identifier,
        asset_name,
        asset.clone(),
    )?;

    let asset_identifier = AssetIdentifier {
        contract_identifier: invoke_ctx.contract_context.contract_identifier.clone(),
        asset_name: asset_name.clone(),
    };
    exec_state.register_nft_burn_event(sender_principal.clone(), asset, asset_identifier)?;

    Ok(Value::okay_true())
}

#[cfg(test)]
mod test {
    use stacks_common::types::StacksEpochId;

    use super::{
        special_burn_asset_v200, special_burn_asset_v205, special_burn_token, special_get_balance,
        special_get_owner_v200, special_get_owner_v205, special_get_token_supply,
        special_mint_asset_v200, special_mint_asset_v205, special_mint_token, special_stx_account,
        special_stx_balance, special_stx_burn, special_stx_transfer, special_stx_transfer_memo,
        special_transfer_asset_v200, special_transfer_asset_v205, special_transfer_token,
    };
    use crate::vm::database::NonFungibleTokenMetadata;
    use crate::vm::errors::{RuntimeCheckErrorKind, VmExecutionError};
    use crate::vm::functions::test_support::special_fn_env;
    use crate::vm::tests::test_clarity_versions;
    use crate::vm::types::{PrincipalData, StandardPrincipalData, TypeSignature};
    use crate::vm::{ClarityName, ClarityVersion, SymbolicExpression, Value};

    fn transient_principal() -> Value {
        Value::Principal(PrincipalData::Standard(StandardPrincipalData::transient()))
    }

    fn unreachable_err(msg: &str) -> VmExecutionError {
        VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::Unreachable(msg.to_string()))
    }

    /// Characterization: locks the exact runtime errors of `special_mint_token`'s
    /// analysis-guaranteed paths, including their precedence (joint value match
    /// happens before the FT metadata lookup).
    #[apply(test_clarity_versions)]
    fn mint_token_unreachable_paths(#[case] version: ClarityVersion, #[case] epoch: StacksEpochId) {
        special_fn_env().run(
            version,
            epoch,
            |_| (),
            |exec_state, invoke_ctx, context| {
                // Non-atom token name.
                let args = [
                    SymbolicExpression::atom_value(Value::UInt(1)),
                    SymbolicExpression::atom_value(Value::UInt(5)),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err = special_mint_token(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad token name"));

                // Wrong value types after eval: joint error, even though the FT metadata
                // lookup (which would also fail here) comes later.
                let args = [
                    SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                    SymbolicExpression::atom_value(Value::Int(5)),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err = special_mint_token(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad mint FT args"));

                // Well-typed args but no such FT defined in the contract context.
                let args = [
                    SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                    SymbolicExpression::atom_value(Value::UInt(5)),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err = special_mint_token(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("No such FT: stackaroo"));
            },
        );
    }

    /// Characterization: locks the exact runtime errors of the transfer-asset twins'
    /// analysis-guaranteed paths, including precedence (NFT metadata lookup before
    /// the admits check, admits before the joint principal match).
    #[apply(test_clarity_versions)]
    fn transfer_asset_unreachable_paths(
        #[case] version: ClarityVersion,
        #[case] epoch: StacksEpochId,
    ) {
        special_fn_env().run(
            version,
            epoch,
            |contract_context| {
                contract_context.meta_nft.insert(
                    ClarityName::from_literal("stackaroo"),
                    NonFungibleTokenMetadata {
                        key_type: TypeSignature::UIntType,
                    },
                );
            },
            |exec_state, invoke_ctx, context| {
                for f in [special_transfer_asset_v200, special_transfer_asset_v205] {
                    // Non-atom asset name.
                    let args = [
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(transient_principal()),
                        SymbolicExpression::atom_value(transient_principal()),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, unreachable_err("Bad token name"));

                    // No such NFT defined in the contract context.
                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("rocket")),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(transient_principal()),
                        SymbolicExpression::atom_value(transient_principal()),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, unreachable_err("No such NFT: rocket"));

                    // Asset value not admitted by the declared key type.
                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                        SymbolicExpression::atom_value(Value::Bool(true)),
                        SymbolicExpression::atom_value(transient_principal()),
                        SymbolicExpression::atom_value(transient_principal()),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(
                        err,
                        VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(
                            Box::new(TypeSignature::UIntType),
                            Value::Bool(true).to_error_string(),
                        ))
                    );

                    // Admitted asset but non-principal from/to: joint error.
                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(Value::UInt(2)),
                        SymbolicExpression::atom_value(transient_principal()),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, unreachable_err("Bad transfer NFT args"));
                }
            },
        );
    }

    /// Characterization: locks the exact runtime errors of the STX operations'
    /// analysis-guaranteed paths (joint value match; principal TypeValueError).
    #[apply(test_clarity_versions)]
    fn stx_ops_unreachable_paths(#[case] version: ClarityVersion, #[case] epoch: StacksEpochId) {
        special_fn_env().run(
            version,
            epoch,
            |_| (),
            |exec_state, invoke_ctx, context| {
                let type_value_err = |v: &Value| {
                    VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::PrincipalType),
                        v.to_error_string(),
                    ))
                };

                // stx-get-balance / stx-account: non-principal owner.
                let args = [SymbolicExpression::atom_value(Value::UInt(1))];
                let err = special_stx_balance(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, type_value_err(&Value::UInt(1)));
                let err = special_stx_account(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, type_value_err(&Value::UInt(1)));

                // stx-transfer?: joint value match failure.
                let args = [
                    SymbolicExpression::atom_value(Value::UInt(5)),
                    SymbolicExpression::atom_value(Value::Int(1)),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err = special_stx_transfer(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad transfer STX args"));

                // stx-transfer-memo?: joint value match failure.
                let args = [
                    SymbolicExpression::atom_value(Value::UInt(5)),
                    SymbolicExpression::atom_value(Value::Int(1)),
                    SymbolicExpression::atom_value(transient_principal()),
                    SymbolicExpression::atom_value(Value::buff_from(vec![1]).unwrap()),
                ];
                let err =
                    special_stx_transfer_memo(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad transfer STX args"));

                // stx-burn?: joint value match failure.
                let args = [
                    SymbolicExpression::atom_value(Value::UInt(5)),
                    SymbolicExpression::atom_value(Value::Int(1)),
                ];
                let err = special_stx_burn(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad transfer STX args"));
            },
        );
    }

    /// Characterization: locks the exact runtime errors of the FT operations'
    /// analysis-guaranteed paths, including precedence (joint/principal checks
    /// before the FT metadata lookup).
    #[apply(test_clarity_versions)]
    fn ft_ops_unreachable_paths(#[case] version: ClarityVersion, #[case] epoch: StacksEpochId) {
        special_fn_env().run(
            version,
            epoch,
            |_| (),
            |exec_state, invoke_ctx, context| {
                // ft-transfer?: non-atom name; joint mismatch; missing metadata.
                let args = [
                    SymbolicExpression::atom_value(Value::UInt(1)),
                    SymbolicExpression::atom_value(Value::UInt(5)),
                    SymbolicExpression::atom_value(transient_principal()),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err =
                    special_transfer_token(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad token name"));

                let args = [
                    SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                    SymbolicExpression::atom_value(Value::Int(5)),
                    SymbolicExpression::atom_value(transient_principal()),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err =
                    special_transfer_token(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad transfer FT args"));

                // ft-burn?: joint mismatch; missing metadata is unreachable via burn
                // (balance check precedes supply), so only the joint error is locked.
                let args = [
                    SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                    SymbolicExpression::atom_value(Value::Int(5)),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err = special_burn_token(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad burn FT args"));

                // ft-get-balance: non-principal owner (before the FT lookup); then
                // missing metadata with a well-typed owner.
                let args = [
                    SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                    SymbolicExpression::atom_value(Value::UInt(1)),
                ];
                let err = special_get_balance(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(
                    err,
                    VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::PrincipalType),
                        Value::UInt(1).to_error_string(),
                    ))
                );
                let args = [
                    SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                    SymbolicExpression::atom_value(transient_principal()),
                ];
                let err = special_get_balance(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("No such FT: stackaroo"));

                // ft-get-supply: non-atom name.
                let args = [SymbolicExpression::atom_value(Value::UInt(1))];
                let err =
                    special_get_token_supply(&args, exec_state, invoke_ctx, context).unwrap_err();
                assert_eq!(err, unreachable_err("Bad token name"));
            },
        );
    }

    /// Characterization: locks the exact runtime errors of the NFT mint /
    /// get-owner / burn twins' analysis-guaranteed paths.
    #[apply(test_clarity_versions)]
    fn nft_ops_unreachable_paths(#[case] version: ClarityVersion, #[case] epoch: StacksEpochId) {
        special_fn_env().run(
            version,
            epoch,
            |contract_context| {
                contract_context.meta_nft.insert(
                    ClarityName::from_literal("stackaroo"),
                    NonFungibleTokenMetadata {
                        key_type: TypeSignature::UIntType,
                    },
                );
            },
            |exec_state, invoke_ctx, context| {
                let uint_type_err = |v: &Value| {
                    VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::UIntType),
                        v.to_error_string(),
                    ))
                };
                let principal_type_err = |v: &Value| {
                    VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::PrincipalType),
                        v.to_error_string(),
                    ))
                };

                // nft-mint? / nft-burn? twins: 3 args (name, asset, principal).
                for f in [
                    special_mint_asset_v200,
                    special_mint_asset_v205,
                    special_burn_asset_v200,
                    special_burn_asset_v205,
                ] {
                    let args = [
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(transient_principal()),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, unreachable_err("Bad token name"));

                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("rocket")),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(transient_principal()),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, unreachable_err("No such NFT: rocket"));

                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                        SymbolicExpression::atom_value(Value::Bool(true)),
                        SymbolicExpression::atom_value(transient_principal()),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, uint_type_err(&Value::Bool(true)));

                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(Value::UInt(2)),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, principal_type_err(&Value::UInt(2)));
                }

                // nft-get-owner? twins: 2 args (name, asset).
                for f in [special_get_owner_v200, special_get_owner_v205] {
                    let args = [
                        SymbolicExpression::atom_value(Value::UInt(1)),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, unreachable_err("Bad token name"));

                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("rocket")),
                        SymbolicExpression::atom_value(Value::UInt(1)),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, unreachable_err("No such NFT: rocket"));

                    let args = [
                        SymbolicExpression::atom(ClarityName::from_literal("stackaroo")),
                        SymbolicExpression::atom_value(Value::Bool(true)),
                    ];
                    let err = f(&args, exec_state, invoke_ctx, context).unwrap_err();
                    assert_eq!(err, uint_type_err(&Value::Bool(true)));
                }
            },
        );
    }
}
