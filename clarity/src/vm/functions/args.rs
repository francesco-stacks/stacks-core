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

//! Typed argument extraction for runtime native/special functions.
//!
//! Special functions receive raw `&[SymbolicExpression]` arguments and must recover
//! facts the static analyzer already proved at deploy time (name atoms, `Value`
//! variants, metadata existence), producing `RuntimeCheckErrorKind::Unreachable` when
//! an "impossible" mismatch occurs. This module centralizes those recoveries so each
//! function body works with typed data and the byte-exact legacy errors live in one
//! audited place.
//!
//! Consensus rules every helper (and every call site conversion) must obey:
//!
//! 1. Cost-charge position varies per function — these are composable primitives
//!    called in each function's *existing* order, never a monolithic entry gateway.
//!    Conversions shorten lines; they never move them.
//! 2. `ValueRef::clone_with_cost` charges `LookupVariableSize` on borrowed refs, so
//!    every extractor yields borrowed views (`&PrincipalData`, `u128`). No helper here
//!    may clone a `Value` or consume a `ValueRef`.
//! 3. Where a function evaluates all arguments and then pattern-matches them jointly
//!    with a single error, use [`typed_refs`]; where it checks one value before
//!    evaluating the next, keep the individual `eval` lines and use [`typed_ref`].
//!    Error precedence must not change.
//! 4. `check_argument_count` calls stay at every call site: some are reachable
//!    (fold/map/filter callbacks, transaction entry) and they produce a distinct
//!    error variant. [`eval_args`] deliberately does not replace them.
//!
//! Rejected alternatives: a declarative `extract_args!` macro (hides eval/cost order
//! from review) and the consuming `Value::expect_*` accessors in clarity-types (they
//! take `self` — forcing a clone — and produce a different error variant).

use crate::vm::analysis::errors::RuntimeCheckErrorKind;
use crate::vm::contexts::{ContractContext, ExecutionState, InvocationContext};
use crate::vm::database::{
    DataMapMetadata, DataVariableMetadata, FungibleTokenMetadata, NonFungibleTokenMetadata,
};
use crate::vm::errors::{VmExecutionError, VmInternalError};
use crate::vm::representations::{ClarityName, SymbolicExpression};
use crate::vm::types::{BuffData, PrincipalData, SequenceData, TypeSignature, Value};
use crate::vm::{LocalContext, ValueRef, eval};

/// Recover a name atom: identical to
/// `expr.match_atom().ok_or(RuntimeCheckErrorKind::Unreachable(<msg>.to_string()))`.
#[inline]
pub fn name_atom<'a>(
    expr: &'a SymbolicExpression,
    msg: &str,
) -> Result<&'a ClarityName, VmExecutionError> {
    expr.match_atom()
        .ok_or_else(|| RuntimeCheckErrorKind::Unreachable(msg.to_string()).into())
}

/// Recover a list of sub-expressions: identical to
/// `expr.match_list().ok_or(RuntimeCheckErrorKind::Unreachable(<msg>.to_string()))`.
#[inline]
pub fn list_exprs<'a>(
    expr: &'a SymbolicExpression,
    msg: &str,
) -> Result<&'a [SymbolicExpression], VmExecutionError> {
    expr.match_list()
        .ok_or_else(|| RuntimeCheckErrorKind::Unreachable(msg.to_string()).into())
}

/// Evaluate exactly `N` argument expressions strictly left-to-right, short-circuiting
/// on the first error — byte-identical to `N` sequential `eval(&args[i], ..)?` lines.
///
/// Callers MUST have already run `check_argument_count`; the arity guard here is an
/// internal invariant (a bug), not a consensus-observable path.
pub fn eval_args<'a, const N: usize>(
    exprs: &'a [SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &'a InvocationContext,
    context: &'a LocalContext,
) -> Result<[ValueRef<'a>; N], VmExecutionError> {
    if exprs.len() != N {
        return Err(VmInternalError::Expect(
            "BUG: eval_args arity mismatch (check_argument_count missing?)".into(),
        )
        .into());
    }
    let mut out = Vec::with_capacity(N);
    for expr in exprs {
        out.push(eval(expr, exec_state, invoke_ctx, context)?);
    }
    out.try_into()
        .map_err(|_| VmInternalError::Expect("BUG: eval_args length mismatch".into()).into())
}

/// A borrowed typed view of one evaluated `Value`. Implementations must not clone.
pub trait ArgType<'v>: Sized {
    fn from_value(v: &'v Value) -> Option<Self>;
}

impl<'v> ArgType<'v> for u128 {
    fn from_value(v: &'v Value) -> Option<Self> {
        if let Value::UInt(x) = v {
            Some(*x)
        } else {
            None
        }
    }
}

impl<'v> ArgType<'v> for &'v PrincipalData {
    fn from_value(v: &'v Value) -> Option<Self> {
        if let Value::Principal(p) = v {
            Some(p)
        } else {
            None
        }
    }
}

impl<'v> ArgType<'v> for &'v BuffData {
    fn from_value(v: &'v Value) -> Option<Self> {
        if let Value::Sequence(SequenceData::Buffer(b)) = v {
            Some(b)
        } else {
            None
        }
    }
}

/// A joint typed view over ALL evaluated values at once. If ANY position mismatches,
/// the caller's single per-function error is produced — same semantics as today's
/// `if let (Value::UInt(a), Value::Principal(b)) = (x.as_ref(), y.as_ref())` blocks.
pub trait ArgTuple<'v>: Sized {
    fn from_values(vals: &'v [ValueRef<'_>]) -> Option<Self>;
}

macro_rules! impl_arg_tuple {
    ($n:expr; $($T:ident $i:tt),+) => {
        impl<'v, $($T: ArgType<'v>),+> ArgTuple<'v> for ($($T,)+) {
            fn from_values(vals: &'v [ValueRef<'_>]) -> Option<Self> {
                if vals.len() != $n {
                    return None;
                }
                Some(($($T::from_value(vals[$i].as_ref())?,)+))
            }
        }
    };
}

impl_arg_tuple!(2; A 0, B 1);
impl_arg_tuple!(3; A 0, B 1, C 2);
impl_arg_tuple!(4; A 0, B 1, C 2, D 3);

/// Joint match over all evaluated values; any mismatch yields the single
/// `on_mismatch` error (see [`ArgTuple`]).
#[inline]
pub fn typed_refs<'v, T: ArgTuple<'v>>(
    vals: &'v [ValueRef<'_>],
    on_mismatch: impl FnOnce() -> RuntimeCheckErrorKind,
) -> Result<T, VmExecutionError> {
    T::from_values(vals).ok_or_else(|| on_mismatch().into())
}

/// Single-value variant for sites whose mismatch error embeds the value
/// (typically `TypeValueError(expected, v.to_error_string())`).
#[inline]
pub fn typed_ref<'v, T: ArgType<'v>>(
    val: &'v Value,
    on_mismatch: impl FnOnce(&Value) -> RuntimeCheckErrorKind,
) -> Result<T, VmExecutionError> {
    T::from_value(val).ok_or_else(|| on_mismatch(val).into())
}

/// The common reachable shape:
/// `TypeValueError(Box::new(PrincipalType), value.to_error_string())`.
#[inline]
pub fn expect_principal_value(v: &Value) -> Result<&PrincipalData, VmExecutionError> {
    typed_ref(v, |v| {
        RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::PrincipalType),
            v.to_error_string(),
        )
    })
}

/// `if !expected.admits(epoch, v)? { TypeValueError(expected, v) }` — the admits-check
/// trio repeated across assets.rs.
#[inline]
pub fn ensure_admits(
    exec_state: &ExecutionState,
    expected: &TypeSignature,
    value: &Value,
) -> Result<(), VmExecutionError> {
    if !expected.admits(exec_state.epoch(), value)? {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(expected.clone()),
            value.to_error_string(),
        )
        .into());
    }
    Ok(())
}

/// Contract-context metadata lookups with the exact legacy error strings.
pub trait ContractContextExt {
    /// `Unreachable("No such FT: {name}")`
    fn ft_info_checked(
        &self,
        name: &ClarityName,
    ) -> Result<&FungibleTokenMetadata, VmExecutionError>;
    /// `Unreachable("No such NFT: {name}")`
    fn nft_info_checked(
        &self,
        name: &ClarityName,
    ) -> Result<&NonFungibleTokenMetadata, VmExecutionError>;
    /// `Unreachable("No such data variable: {name}")`
    fn data_var_checked(
        &self,
        name: &ClarityName,
    ) -> Result<&DataVariableMetadata, VmExecutionError>;
    /// `Unreachable("No such map: {name}")`
    fn data_map_checked(&self, name: &ClarityName) -> Result<&DataMapMetadata, VmExecutionError>;
}

impl ContractContextExt for ContractContext {
    fn ft_info_checked(
        &self,
        name: &ClarityName,
    ) -> Result<&FungibleTokenMetadata, VmExecutionError> {
        self.meta_ft
            .get(name)
            .ok_or_else(|| RuntimeCheckErrorKind::Unreachable(format!("No such FT: {name}")).into())
    }

    fn nft_info_checked(
        &self,
        name: &ClarityName,
    ) -> Result<&NonFungibleTokenMetadata, VmExecutionError> {
        self.meta_nft.get(name).ok_or_else(|| {
            RuntimeCheckErrorKind::Unreachable(format!("No such NFT: {name}")).into()
        })
    }

    fn data_var_checked(
        &self,
        name: &ClarityName,
    ) -> Result<&DataVariableMetadata, VmExecutionError> {
        self.meta_data_var.get(name).ok_or_else(|| {
            RuntimeCheckErrorKind::Unreachable(format!("No such data variable: {name}")).into()
        })
    }

    fn data_map_checked(&self, name: &ClarityName) -> Result<&DataMapMetadata, VmExecutionError> {
        self.meta_data_map.get(name).ok_or_else(|| {
            RuntimeCheckErrorKind::Unreachable(format!("No such map: {name}")).into()
        })
    }
}

/// `Unreachable("Write attempted in read-only")` — the guard repeated across
/// database.rs write paths.
#[inline]
pub fn ensure_not_read_only(exec_state: &ExecutionState) -> Result<(), VmExecutionError> {
    if exec_state.global_context.is_read_only() {
        Err(RuntimeCheckErrorKind::Unreachable("Write attempted in read-only".to_string()).into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use stacks_common::consts::CHAIN_ID_TESTNET;
    use stacks_common::types::StacksEpochId;

    use super::*;
    use crate::vm::costs::LimitedCostTracker;
    use crate::vm::database::MemoryBackingStore;
    use crate::vm::types::{QualifiedContractIdentifier, StandardPrincipalData};
    use crate::vm::{CallStack, ClarityVersion, GlobalContext};

    fn unreachable_err(msg: &str) -> VmExecutionError {
        VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::Unreachable(msg.to_string()))
    }

    #[test]
    fn name_atom_recovers_atom_and_reproduces_legacy_errors() {
        let atom = SymbolicExpression::atom(ClarityName::from_literal("stackaroo"));
        assert_eq!(
            name_atom(&atom, "Bad token name").unwrap(),
            &ClarityName::from_literal("stackaroo")
        );

        let non_atom = SymbolicExpression::atom_value(Value::UInt(1));
        assert_eq!(
            name_atom(&non_atom, "Bad token name").unwrap_err(),
            unreachable_err("Bad token name")
        );
        assert_eq!(
            name_atom(&non_atom, "Expected name").unwrap_err(),
            unreachable_err("Expected name")
        );
        assert_eq!(
            name_atom(&non_atom, "Bad match option syntax: expected name").unwrap_err(),
            unreachable_err("Bad match option syntax: expected name")
        );
        assert_eq!(
            name_atom(&non_atom, "Bad match response syntax: expected name").unwrap_err(),
            unreachable_err("Bad match response syntax: expected name")
        );
    }

    #[test]
    fn list_exprs_recovers_list_and_reproduces_legacy_error() {
        let list = SymbolicExpression::list(vec![SymbolicExpression::atom_value(Value::UInt(1))]);
        assert_eq!(
            list_exprs(&list, "Non functional application")
                .unwrap()
                .len(),
            1
        );
        let non_list = SymbolicExpression::atom_value(Value::UInt(1));
        assert_eq!(
            list_exprs(&non_list, "Non functional application").unwrap_err(),
            unreachable_err("Non functional application")
        );
    }

    #[test]
    fn typed_refs_joint_match_and_single_error() {
        let principal =
            Value::Principal(PrincipalData::Standard(StandardPrincipalData::transient()));
        let vals = [ValueRef::Owned(Value::UInt(10)), ValueRef::Owned(principal)];

        let (amount, to): (u128, &PrincipalData) = typed_refs(&vals, || {
            RuntimeCheckErrorKind::Unreachable("nope".to_string())
        })
        .unwrap();
        assert_eq!(amount, 10);
        assert_eq!(
            to,
            &PrincipalData::Standard(StandardPrincipalData::transient())
        );

        // ANY position mismatching yields the single joint error — even when a
        // later position matches (jointness, not per-position precedence).
        let bad = [
            ValueRef::Owned(Value::Int(10)),
            ValueRef::Owned(Value::UInt(1)),
        ];
        let res: Result<(u128, &PrincipalData), _> = typed_refs(&bad, || {
            RuntimeCheckErrorKind::Unreachable("Bad mint FT args".to_string())
        });
        assert_eq!(res.unwrap_err(), unreachable_err("Bad mint FT args"));

        // Length mismatch is also the joint error, never a panic.
        let one = [ValueRef::Owned(Value::UInt(1))];
        let res: Result<(u128, &PrincipalData), _> = typed_refs(&one, || {
            RuntimeCheckErrorKind::Unreachable("Bad mint FT args".to_string())
        });
        assert_eq!(res.unwrap_err(), unreachable_err("Bad mint FT args"));
    }

    #[test]
    fn arg_type_views_are_borrowed_and_exhaustive() {
        let buff = Value::buff_from(vec![1, 2, 3]).unwrap();
        assert!(<&BuffData>::from_value(&buff).is_some());
        assert!(<&BuffData>::from_value(&Value::UInt(1)).is_none());

        assert_eq!(u128::from_value(&Value::UInt(7)), Some(7));
        assert_eq!(u128::from_value(&Value::Int(7)), None);
    }

    #[test]
    fn expect_principal_value_reproduces_type_value_error() {
        let owner = Value::UInt(99);
        assert_eq!(
            expect_principal_value(&owner).unwrap_err(),
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(
                Box::new(TypeSignature::PrincipalType),
                owner.to_error_string(),
            ))
        );

        let principal =
            Value::Principal(PrincipalData::Standard(StandardPrincipalData::transient()));
        assert!(expect_principal_value(&principal).is_ok());
    }

    #[test]
    fn contract_context_checked_lookups_reproduce_legacy_errors() {
        let mut contract_context = ContractContext::new(
            QualifiedContractIdentifier::transient(),
            ClarityVersion::latest(),
        );
        contract_context.meta_ft.insert(
            ClarityName::from_literal("stackaroo"),
            FungibleTokenMetadata { total_supply: None },
        );

        assert!(
            contract_context
                .ft_info_checked(&ClarityName::from_literal("stackaroo"))
                .is_ok()
        );
        assert_eq!(
            contract_context
                .ft_info_checked(&ClarityName::from_literal("missing"))
                .unwrap_err(),
            unreachable_err("No such FT: missing")
        );
        assert_eq!(
            contract_context
                .nft_info_checked(&ClarityName::from_literal("missing"))
                .unwrap_err(),
            unreachable_err("No such NFT: missing")
        );
        assert_eq!(
            contract_context
                .data_var_checked(&ClarityName::from_literal("missing"))
                .unwrap_err(),
            unreachable_err("No such data variable: missing")
        );
        assert_eq!(
            contract_context
                .data_map_checked(&ClarityName::from_literal("missing"))
                .unwrap_err(),
            unreachable_err("No such map: missing")
        );
    }

    #[test]
    fn eval_args_and_state_guards() {
        let mut marf = MemoryBackingStore::new();
        let mut global_context = GlobalContext::new(
            false,
            CHAIN_ID_TESTNET,
            marf.as_clarity_db(),
            LimitedCostTracker::new_free(),
            StacksEpochId::latest(),
        );
        let contract_context = ContractContext::new(
            QualifiedContractIdentifier::transient(),
            ClarityVersion::latest(),
        );
        let context = LocalContext::new();
        let mut call_stack = CallStack::new();
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

        let exprs = [
            SymbolicExpression::atom_value(Value::UInt(1)),
            SymbolicExpression::atom_value(Value::Bool(false)),
        ];
        let vals: [ValueRef; 2] =
            eval_args(&exprs, &mut exec_state, &invoke_ctx, &context).unwrap();
        assert_eq!(vals[0].as_ref(), &Value::UInt(1));
        assert_eq!(vals[1].as_ref(), &Value::Bool(false));

        // Internal arity mismatch is a bug-class error, not a consensus error.
        let res: Result<[ValueRef; 3], _> =
            eval_args(&exprs, &mut exec_state, &invoke_ctx, &context);
        assert_eq!(
            res.unwrap_err(),
            VmExecutionError::Internal(VmInternalError::Expect(
                "BUG: eval_args arity mismatch (check_argument_count missing?)".into()
            ))
        );

        // Not read-only outside a read-only context.
        assert!(ensure_not_read_only(&exec_state).is_ok());

        // admits: uint value against principal type is the legacy TypeValueError.
        let val = Value::UInt(3);
        assert_eq!(
            ensure_admits(&exec_state, &TypeSignature::PrincipalType, &val).unwrap_err(),
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(
                Box::new(TypeSignature::PrincipalType),
                val.to_error_string(),
            ))
        );
        assert!(ensure_admits(&exec_state, &TypeSignature::UIntType, &val).is_ok());
    }

    #[test]
    fn ensure_not_read_only_reproduces_legacy_error() {
        let mut marf = MemoryBackingStore::new();
        let mut global_context = GlobalContext::new(
            false,
            CHAIN_ID_TESTNET,
            marf.as_clarity_db(),
            LimitedCostTracker::new_free(),
            StacksEpochId::latest(),
        );
        global_context.begin_read_only();
        let mut call_stack = CallStack::new();
        let exec_state = ExecutionState {
            global_context: &mut global_context,
            call_stack: &mut call_stack,
        };
        assert_eq!(
            ensure_not_read_only(&exec_state).unwrap_err(),
            unreachable_err("Write attempted in read-only")
        );
    }
}
