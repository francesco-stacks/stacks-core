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

//! Typed evaluator over [`LExpr`] (Phase-0).
//!
//! Parity contract with the legacy tree-walker (asserted by the dual-path
//! harness): identical results, errors, events, and cost/memory charge
//! sequences. The rules replicated here, with their legacy sources:
//! - `Opaque` subtrees run the unchanged legacy `eval`.
//! - Every call node charges `LookupFunction` first (vm::lookup_function).
//! - `check_call_preconditions` runs before argument evaluation; eager
//!   arguments replay `apply`'s per-arg eval → clone_with_cost →
//!   get_memory_use → add_memory sequence with its exact error unwinding,
//!   then converge on the shared `dispatch_args` (call-stack insert before
//!   the native cost charge; NativeFunction205 epoch gate inside).
//! - Special forms reproduce `apply`'s special branch bookkeeping
//!   (insert → core → add_stack_trace → remove) and their cores replicate
//!   the legacy special functions statement by statement.
//! - Phase-0 resolves `CallableType` through `lookup_reserved_functions` at
//!   runtime — identical work to legacy, so parity is trivial; memoization
//!   is a later, benchmarked optimization.

use stacks_common::types::StacksEpochId;

use super::nodes::{LCall, LCallKind, LExpr};
use crate::vm::callables::CallableType;
use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::{CostTracker, MemoryConsumer, runtime_cost};
use crate::vm::errors::{
    EarlyReturnError, RuntimeCheckErrorKind, VmExecutionError, VmInternalError,
};
use crate::vm::functions::lookup_reserved_functions;
use crate::vm::types::{TupleData, TypeSignature, Value};
use crate::vm::{
    ClarityVersion, LocalContext, ValueRef, add_stack_trace, check_call_preconditions,
    check_interpreter_abort_condition, dispatch_args, eval, is_reserved, lookup_variable,
};

pub fn eval_lowered<'a>(
    expr: &'a LExpr,
    exec_state: &mut ExecutionState,
    invoke_ctx: &'a InvocationContext,
    context: &'a LocalContext,
) -> Result<ValueRef<'a>, VmExecutionError> {
    // Opaque delegates to legacy eval, which performs its own entry abort
    // check — checking here too would double the cadence for that node.
    if let LExpr::Opaque(e) = expr {
        return eval(e, exec_state, invoke_ctx, context);
    }

    // Mirrors legacy eval's per-node abort-condition cadence. Eval hooks are
    // not fired here: the lowered path is only entered with hooks disabled.
    check_interpreter_abort_condition(exec_state.global_context)?;

    match expr {
        LExpr::Literal(value) => Ok(ValueRef::Owned(value.clone())),
        LExpr::Var(name) => lookup_variable(name, exec_state, invoke_ctx, context),
        LExpr::Opaque(..) => unreachable!("handled above"),
        LExpr::Call(call) => eval_call(call, exec_state, invoke_ctx, context),
    }
}

fn eval_call<'a>(
    call: &'a LCall,
    exec_state: &mut ExecutionState,
    invoke_ctx: &'a InvocationContext,
    context: &'a LocalContext,
) -> Result<ValueRef<'a>, VmExecutionError> {
    // lookup_function charges before resolving; keep the position.
    runtime_cost(ClarityCostFunction::LookupFunction, exec_state, 0)?;

    let version = invoke_ctx.contract_context.get_clarity_version();
    let callable = match &call.kind {
        LCallKind::Native { func, .. } => {
            lookup_reserved_functions(func.get_name_str(), version)
                .ok_or_else(|| VmInternalError::Expect("BUG: lowered native must resolve".into()))?
        }
        LCallKind::User { name, .. } => {
            let function = invoke_ctx
                .contract_context
                .lookup_function(name)
                .ok_or_else(|| RuntimeCheckErrorKind::UndefinedFunction(name.to_string()))?;
            CallableType::UserFunction(function)
        }
        LCallKind::If { .. } => resolve_special("if", version)?,
        LCallKind::Asserts { .. } => resolve_special("asserts!", version)?,
        LCallKind::Print { .. } => resolve_special("print", version)?,
        LCallKind::TupleGet { .. } => resolve_special("get", version)?,
        LCallKind::TupleCons { .. } => resolve_special("tuple", version)?,
        LCallKind::Let { .. } => resolve_special("let", version)?,
        LCallKind::VarGet { .. } => resolve_special("var-get", version)?,
        LCallKind::VarSet { .. } => resolve_special("var-set", version)?,
    };

    let (identifier, track_recursion) = check_call_preconditions(&callable, exec_state)?;

    match &call.kind {
        LCallKind::Native { args, .. } | LCallKind::User { args, .. } => {
            // Replays apply's eager-argument loop byte for byte.
            let mut used_memory = 0;
            let mut evaluated_args = Vec::with_capacity(args.len());
            exec_state.call_stack.incr_apply_depth();
            for arg_x in args.iter() {
                let arg_value = match eval_lowered(arg_x, exec_state, invoke_ctx, context)
                    .and_then(|v| v.clone_with_cost(exec_state))
                {
                    Ok(x) => x,
                    Err(e) => {
                        exec_state.drop_memory(used_memory)?;
                        exec_state.call_stack.decr_apply_depth();
                        return Err(e);
                    }
                };
                let arg_use = match arg_value.get_memory_use() {
                    Ok(x) => x,
                    Err(e) => {
                        exec_state.drop_memory(used_memory)?;
                        exec_state.call_stack.decr_apply_depth();
                        return Err(e.into());
                    }
                };
                match exec_state.add_memory(arg_use) {
                    Ok(_x) => {}
                    Err(e) => {
                        exec_state.drop_memory(used_memory)?;
                        exec_state.call_stack.decr_apply_depth();
                        return Err(VmExecutionError::from(e));
                    }
                };
                used_memory += arg_use;
                evaluated_args.push(arg_value);
            }
            exec_state.call_stack.decr_apply_depth();

            dispatch_args(
                &callable,
                identifier,
                track_recursion,
                evaluated_args,
                used_memory,
                exec_state,
                invoke_ctx,
            )
            .map(ValueRef::Owned)
        }
        kind => {
            // apply's special branch: insert → core → stack trace → remove.
            exec_state.call_stack.insert(&identifier, track_recursion);
            let mut resp = eval_special(kind, exec_state, invoke_ctx, context);
            add_stack_trace(&mut resp, exec_state);
            exec_state.call_stack.remove(&identifier, track_recursion)?;
            resp.map(ValueRef::Owned)
        }
    }
}

fn resolve_special(name: &str, version: &ClarityVersion) -> Result<CallableType, VmExecutionError> {
    lookup_reserved_functions(name, version)
        .ok_or_else(|| VmInternalError::Expect("BUG: lowered special must resolve".into()).into())
}

fn eval_special(
    kind: &LCallKind,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    match kind {
        // Core of functions::special_if (arity proven at lower time).
        LCallKind::If {
            cond,
            then_expr,
            else_expr,
        } => {
            runtime_cost(ClarityCostFunction::If, exec_state, 0)?;
            let conditional = eval_lowered(cond, exec_state, invoke_ctx, context)?;
            match conditional.as_ref() {
                Value::Bool(result) => {
                    if *result {
                        eval_lowered(then_expr, exec_state, invoke_ctx, context)?
                            .clone_with_cost(exec_state)
                    } else {
                        eval_lowered(else_expr, exec_state, invoke_ctx, context)?
                            .clone_with_cost(exec_state)
                    }
                }
                _ => Err(RuntimeCheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BoolType),
                    conditional.as_ref().to_error_string(),
                )
                .into()),
            }
        }
        // Core of functions::special_asserts (arity proven at lower time).
        LCallKind::Asserts { cond, thrown } => {
            runtime_cost(ClarityCostFunction::Asserts, exec_state, 0)?;
            let conditional = eval_lowered(cond, exec_state, invoke_ctx, context)?;
            match conditional.as_ref() {
                Value::Bool(result) => {
                    if *result {
                        conditional.clone_with_cost(exec_state)
                    } else {
                        let thrown_value = eval_lowered(thrown, exec_state, invoke_ctx, context)?
                            .clone_with_cost(exec_state)?;
                        Err(EarlyReturnError::AssertionFailed(Box::new(thrown_value)).into())
                    }
                }
                _ => Err(RuntimeCheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BoolType),
                    conditional.as_ref().to_error_string(),
                )
                .into()),
            }
        }
        // Core of functions::special_print (single-argument shape proven).
        LCallKind::Print { value } => {
            let input = eval_lowered(value, exec_state, invoke_ctx, context)?;
            runtime_cost(
                ClarityCostFunction::Print,
                exec_state,
                input.as_ref().size()?,
            )?;
            if cfg!(feature = "developer-mode") {
                debug!("{}", input.as_ref());
            }
            let value = input.clone_with_cost(exec_state)?;
            exec_state.register_print_event(invoke_ctx, value.clone())?;
            Ok(value)
        }
        // Core of tuples::tuple_get (name shape proven; the whole value is
        // cloned before the TupleGet cost, exactly like legacy).
        LCallKind::TupleGet { field, tuple } => {
            let value = eval_lowered(tuple, exec_state, invoke_ctx, context)?;
            match value.clone_with_cost(exec_state)? {
                Value::Optional(opt_data) => match opt_data.data {
                    Some(data) => {
                        if let Value::Tuple(tuple_data) = *data {
                            runtime_cost(
                                ClarityCostFunction::TupleGet,
                                exec_state,
                                tuple_data.len(),
                            )?;
                            Ok(Value::some(tuple_data.get_owned(field)?).map_err(|_| {
                                VmInternalError::Expect(
                                    "Tuple contents should *always* fit in a some wrapper".into(),
                                )
                            })?)
                        } else {
                            Err(RuntimeCheckErrorKind::Unreachable(format!(
                                "Expected tuple: {}",
                                TypeSignature::type_of(&data)?
                            ))
                            .into())
                        }
                    }
                    None => Ok(Value::none()),
                },
                Value::Tuple(tuple_data) => {
                    runtime_cost(ClarityCostFunction::TupleGet, exec_state, tuple_data.len())?;
                    Ok(tuple_data.get_owned(field)?)
                }
                other_value => Err(RuntimeCheckErrorKind::Unreachable(format!(
                    "Expected tuple: {}",
                    TypeSignature::type_of(&other_value)?
                ))
                .into()),
            }
        }
        // Core of tuples::tuple_cons via parse_eval_bindings: eval +
        // clone_with_cost per binding, then the TupleCons cost.
        LCallKind::TupleCons { bindings } => {
            let mut data = Vec::with_capacity(bindings.len());
            for (name, value_expr) in bindings {
                let value = eval_lowered(value_expr, exec_state, invoke_ctx, context)?
                    .clone_with_cost(exec_state)?;
                data.push((name.clone(), value));
            }
            runtime_cost(ClarityCostFunction::TupleCons, exec_state, data.len())?;
            Ok(TupleData::from_data(data).map(Value::from)?)
        }
        // Core of functions::special_let (binding shapes proven at lower
        // time; the runtime-dependent name-collision checks stay).
        LCallKind::Let { bindings, body } => {
            runtime_cost(ClarityCostFunction::Let, exec_state, bindings.len())?;

            let mut inner_context = context.extend()?;
            let mut memory_use = 0;

            let result = eval_let_inner(
                bindings,
                body,
                &mut inner_context,
                &mut memory_use,
                exec_state,
                invoke_ctx,
            );
            // finally_drop_memory!: released on success and error alike.
            exec_state.drop_memory(memory_use)?;
            result
        }
        // Cores of database::special_fetch_variable_v200/v205, epoch-gated
        // like switch_on_global_epoch.
        LCallKind::VarGet { var, meta } => {
            let contract = &invoke_ctx.contract_context.contract_identifier;
            // Replicates switch_on_global_epoch: Epoch10 panics before any
            // core runs, Epoch20 selects v200, every later epoch v205.
            let epoch = *exec_state.epoch();
            if epoch == StacksEpochId::Epoch10 {
                panic!("Executing Clarity method during Epoch 1.0, before Clarity")
            }
            if epoch != StacksEpochId::Epoch20 {
                let result = exec_state
                    .global_context
                    .database
                    .lookup_variable_with_size(contract, var, meta, &epoch);
                let result_size = match &result {
                    Ok(data) => data.serialized_byte_len,
                    Err(_e) => meta.value_type.size()?.into(),
                };
                runtime_cost(ClarityCostFunction::FetchVar, exec_state, result_size)?;
                result.map(|data| data.value)
            } else {
                runtime_cost(
                    ClarityCostFunction::FetchVar,
                    exec_state,
                    meta.value_type.size()?,
                )?;
                exec_state
                    .global_context
                    .database
                    .lookup_variable(contract, var, meta, &epoch)
            }
        }
        // Cores of database::special_set_variable_v200/v205.
        LCallKind::VarSet { var, meta, value } => {
            let epoch = *exec_state.epoch();
            if epoch == StacksEpochId::Epoch10 {
                panic!("Executing Clarity method during Epoch 1.0, before Clarity")
            }
            if exec_state.global_context.is_read_only() {
                return Err(RuntimeCheckErrorKind::Unreachable(
                    "Write attempted in read-only".to_string(),
                )
                .into());
            }
            let value = eval_lowered(value, exec_state, invoke_ctx, context)?;
            let contract = &invoke_ctx.contract_context.contract_identifier;
            // switch_on_global_epoch: Epoch20 runs the v200 core, every
            // later epoch the v205 core (Epoch10 panicked above).
            if epoch != StacksEpochId::Epoch20 {
                let value = value.clone_with_cost(exec_state)?;
                let result = exec_state
                    .global_context
                    .database
                    .set_variable(contract, var, value, meta, &epoch);
                let result_size = match &result {
                    Ok(data) => data.serialized_byte_len,
                    Err(_e) => meta.value_type.size()?.into(),
                };
                runtime_cost(ClarityCostFunction::SetVar, exec_state, result_size)?;
                exec_state.add_memory(result_size)?;
                result.map(|data| data.value)
            } else {
                runtime_cost(
                    ClarityCostFunction::SetVar,
                    exec_state,
                    meta.value_type.size()?,
                )?;
                exec_state.add_memory(value.as_ref().get_memory_use()?)?;
                let value = value.clone_with_cost(exec_state)?;
                exec_state
                    .global_context
                    .database
                    .set_variable(contract, var, value, meta, &epoch)
                    .map(|data| data.value)
            }
        }
        LCallKind::Native { .. } | LCallKind::User { .. } => {
            Err(VmInternalError::Expect("BUG: eager kind in eval_special".into()).into())
        }
    }
}

fn eval_let_inner(
    bindings: &[(crate::vm::ClarityName, LExpr)],
    body: &[LExpr],
    inner_context: &mut LocalContext,
    memory_use: &mut u64,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
) -> Result<Value, VmExecutionError> {
    let version = invoke_ctx.contract_context.get_clarity_version();
    for (binding_name, value_expr) in bindings {
        if is_reserved(binding_name, version)
            || invoke_ctx
                .contract_context
                .lookup_function(binding_name)
                .is_some()
            || inner_context.lookup_variable(binding_name).is_some()
        {
            return Err(RuntimeCheckErrorKind::NameAlreadyUsed(binding_name.clone().into()).into());
        }

        let binding_value = eval_lowered(value_expr, exec_state, invoke_ctx, inner_context)?;

        let bind_mem_use = binding_value.as_ref().get_memory_use()?;
        exec_state.add_memory(bind_mem_use)?;
        *memory_use += bind_mem_use;
        let binding_value = binding_value.clone_with_cost(exec_state)?;
        if *version >= ClarityVersion::Clarity2
            && let Value::CallableContract(trait_data) = &binding_value
        {
            inner_context
                .callable_contracts
                .insert(binding_name.clone(), trait_data.clone());
        }
        inner_context
            .variables
            .insert(binding_name.clone(), binding_value);
    }

    let mut last_result = None;
    for body_expr in body.iter() {
        let body_result = eval_lowered(body_expr, exec_state, invoke_ctx, inner_context)?;
        last_result.replace(body_result);
    }
    last_result
        .ok_or_else(|| {
            VmExecutionError::from(VmInternalError::Expect("Failed to get let result".into()))
        })?
        .clone_with_cost(exec_state)
}

#[cfg(test)]
mod test {
    use stacks_common::consts::CHAIN_ID_TESTNET;

    use super::*;
    use crate::vm::costs::LimitedCostTracker;
    use crate::vm::database::{DataVariableMetadata, MemoryBackingStore};
    use crate::vm::lowered::lower_function_body;
    use crate::vm::representations::SymbolicExpression;
    use crate::vm::types::QualifiedContractIdentifier;
    use crate::vm::{CallStack, ClarityName, ContractContext, GlobalContext};

    fn atom(s: &'static str) -> SymbolicExpression {
        SymbolicExpression::atom(ClarityName::from_literal(s))
    }

    /// Evaluate `expr` through legacy eval and through lower+eval_lowered in
    /// identical fresh environments; assert identical outcomes and return them.
    pub fn assert_paths_agree(
        expr: &SymbolicExpression,
        read_only: bool,
    ) -> Result<Value, VmExecutionError> {
        assert_paths_agree_at_budget(expr, read_only, None)
    }

    /// Runs both evaluators; `runtime_budget` swaps the free tracker for a
    /// real `LimitedCostTracker` with that runtime allowance, so sweeping
    /// budgets makes the comparison sensitive to charge ORDER: a divergent
    /// sequence exhausts at a different point and changes the outcome.
    pub fn assert_paths_agree_at_budget(
        expr: &SymbolicExpression,
        read_only: bool,
        runtime_budget: Option<u64>,
    ) -> Result<Value, VmExecutionError> {
        let mut outcomes = Vec::new();
        for lowered_path in [false, true] {
            let mut marf = MemoryBackingStore::new();
            let mut global_context = GlobalContext::new(
                false,
                CHAIN_ID_TESTNET,
                marf.as_clarity_db(),
                match runtime_budget {
                    None => LimitedCostTracker::new_free(),
                    Some(budget) => {
                        let mut limit = crate::vm::costs::ExecutionCost::max_value();
                        limit.runtime = budget;
                        LimitedCostTracker::new_with_limit(StacksEpochId::latest(), limit)
                    }
                },
                StacksEpochId::latest(),
            );
            let mut contract_context = ContractContext::new(
                QualifiedContractIdentifier::transient(),
                ClarityVersion::latest(),
            );
            contract_context.meta_data_var.insert(
                ClarityName::from_literal("cursor"),
                DataVariableMetadata {
                    value_type: crate::vm::types::TypeSignature::UIntType,
                },
            );
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
            let outcome = if lowered_path {
                let lowered = lower_function_body(expr, &contract_context);
                assert!(
                    !matches!(lowered, super::LExpr::Opaque(_)),
                    "test expr must lower to a typed node: {expr}"
                );
                eval_lowered(&lowered, &mut exec_state, &invoke_ctx, &context)
                    .and_then(|v| v.clone_with_cost(&mut exec_state))
            } else {
                eval(expr, &mut exec_state, &invoke_ctx, &context)
                    .and_then(|v| v.clone_with_cost(&mut exec_state))
            };
            let events: Vec<_> = global_context
                .event_batches
                .iter()
                .map(|(batch, size)| (batch.events.clone(), *size))
                .collect();
            outcomes.push((outcome, events));
        }
        let (lowered_outcome, lowered_events) = outcomes.pop().unwrap();
        let (legacy_outcome, legacy_events) = outcomes.pop().unwrap();
        assert_eq!(legacy_outcome, lowered_outcome);
        assert_eq!(legacy_events, lowered_events);
        legacy_outcome
    }

    #[test]
    fn if_non_bool_condition_matches_legacy() {
        let expr = SymbolicExpression::list(vec![
            atom("if"),
            SymbolicExpression::atom_value(Value::UInt(1)),
            SymbolicExpression::atom_value(Value::UInt(2)),
            SymbolicExpression::atom_value(Value::UInt(3)),
        ]);
        let err = assert_paths_agree(&expr, false).unwrap_err();
        assert!(matches!(
            err,
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(..))
        ));
    }

    #[test]
    fn let_reserved_name_collision_matches_legacy() {
        // (let ((tx-sender u1)) u2) — binding a reserved name.
        let expr = SymbolicExpression::list(vec![
            atom("let"),
            SymbolicExpression::list(vec![SymbolicExpression::list(vec![
                atom("tx-sender"),
                SymbolicExpression::atom_value(Value::UInt(1)),
            ])]),
            SymbolicExpression::atom_value(Value::UInt(2)),
        ]);
        let err = assert_paths_agree(&expr, false).unwrap_err();
        assert!(matches!(
            err,
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::NameAlreadyUsed(..))
        ));
    }

    #[test]
    fn let_shadowing_and_happy_path_match_legacy() {
        // (let ((a u1) (b u2)) (if true a b))
        let expr = SymbolicExpression::list(vec![
            atom("let"),
            SymbolicExpression::list(vec![
                SymbolicExpression::list(vec![
                    atom("a"),
                    SymbolicExpression::atom_value(Value::UInt(1)),
                ]),
                SymbolicExpression::list(vec![
                    atom("b"),
                    SymbolicExpression::atom_value(Value::UInt(2)),
                ]),
            ]),
            SymbolicExpression::list(vec![
                atom("if"),
                SymbolicExpression::atom_value(Value::Bool(true)),
                atom("a"),
                atom("b"),
            ]),
        ]);
        assert_eq!(assert_paths_agree(&expr, false).unwrap(), Value::UInt(1));

        // (let ((a u1) (a u2)) a) — duplicate binding collides at runtime.
        let expr = SymbolicExpression::list(vec![
            atom("let"),
            SymbolicExpression::list(vec![
                SymbolicExpression::list(vec![
                    atom("a"),
                    SymbolicExpression::atom_value(Value::UInt(1)),
                ]),
                SymbolicExpression::list(vec![
                    atom("a"),
                    SymbolicExpression::atom_value(Value::UInt(2)),
                ]),
            ]),
            atom("a"),
        ]);
        let err = assert_paths_agree(&expr, false).unwrap_err();
        assert!(matches!(
            err,
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::NameAlreadyUsed(..))
        ));
    }

    #[test]
    fn var_ops_match_legacy_including_read_only_guard() {
        // (var-set cursor u5) in a writable frame: both paths agree (the
        // fresh store has no such var row, so both hit the same DB error or
        // success — equality is what matters).
        let set_expr = SymbolicExpression::list(vec![
            atom("var-set"),
            atom("cursor"),
            SymbolicExpression::atom_value(Value::UInt(5)),
        ]);
        let _ = assert_paths_agree(&set_expr, false);

        // Same write under a read-only frame: the leading guard fires.
        let err = assert_paths_agree(&set_expr, true).unwrap_err();
        assert_eq!(
            err,
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::Unreachable(
                "Write attempted in read-only".to_string()
            ))
        );

        // (var-get cursor): both paths agree.
        let get_expr = SymbolicExpression::list(vec![atom("var-get"), atom("cursor")]);
        let _ = assert_paths_agree(&get_expr, false);
    }
}

#[cfg(test)]
mod tier1_test {
    use super::test::*;
    use super::*;
    use crate::vm::representations::SymbolicExpression;

    fn atom(s: &'static str) -> SymbolicExpression {
        SymbolicExpression::atom(crate::vm::ClarityName::from_literal(s))
    }

    fn pair(name: &'static str, v: Value) -> SymbolicExpression {
        SymbolicExpression::list(vec![atom(name), SymbolicExpression::atom_value(v)])
    }

    #[test]
    fn asserts_matches_legacy() {
        // true condition returns the bool.
        let expr = SymbolicExpression::list(vec![
            atom("asserts!"),
            SymbolicExpression::atom_value(Value::Bool(true)),
            SymbolicExpression::atom_value(Value::UInt(9)),
        ]);
        assert_eq!(assert_paths_agree(&expr, false).unwrap(), Value::Bool(true));

        // false condition throws the (lazily evaluated) thrown value.
        let expr = SymbolicExpression::list(vec![
            atom("asserts!"),
            SymbolicExpression::atom_value(Value::Bool(false)),
            SymbolicExpression::atom_value(Value::UInt(9)),
        ]);
        let err = assert_paths_agree(&expr, false).unwrap_err();
        assert!(matches!(
            err,
            VmExecutionError::EarlyReturn(EarlyReturnError::AssertionFailed(..))
        ));

        // non-bool condition is the legacy TypeValueError.
        let expr = SymbolicExpression::list(vec![
            atom("asserts!"),
            SymbolicExpression::atom_value(Value::UInt(1)),
            SymbolicExpression::atom_value(Value::UInt(9)),
        ]);
        let err = assert_paths_agree(&expr, false).unwrap_err();
        assert!(matches!(
            err,
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::TypeValueError(..))
        ));
    }

    #[test]
    fn print_matches_legacy_including_events() {
        // assert_paths_agree compares emitted event batches, which is the
        // load-bearing part for print.
        let expr = SymbolicExpression::list(vec![
            atom("print"),
            SymbolicExpression::atom_value(Value::UInt(5)),
        ]);
        assert_eq!(assert_paths_agree(&expr, false).unwrap(), Value::UInt(5));
    }

    #[test]
    fn tuple_get_and_cons_match_legacy() {
        // (get a (tuple (a u1) (b u2))) -> u1
        let tuple_expr = SymbolicExpression::list(vec![
            atom("tuple"),
            pair("a", Value::UInt(1)),
            pair("b", Value::UInt(2)),
        ]);
        let expr = SymbolicExpression::list(vec![atom("get"), atom("a"), tuple_expr.clone()]);
        assert_eq!(assert_paths_agree(&expr, false).unwrap(), Value::UInt(1));

        // (get a (some (tuple ...))) -> (some u1); (get a none) -> none
        let expr = SymbolicExpression::list(vec![
            atom("get"),
            atom("a"),
            SymbolicExpression::list(vec![atom("some"), tuple_expr]),
        ]);
        assert_eq!(
            assert_paths_agree(&expr, false).unwrap(),
            Value::some(Value::UInt(1)).unwrap()
        );
        let expr = SymbolicExpression::list(vec![atom("get"), atom("a"), atom("none")]);
        assert_eq!(assert_paths_agree(&expr, false).unwrap(), Value::none());

        // (get a u1): the legacy "Expected tuple: uint" unreachable guard.
        let expr = SymbolicExpression::list(vec![
            atom("get"),
            atom("a"),
            SymbolicExpression::atom_value(Value::UInt(1)),
        ]);
        let err = assert_paths_agree(&expr, false).unwrap_err();
        assert!(matches!(
            err,
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::Unreachable(..))
        ));

        // (get missing (tuple (a u1))): missing field errors identically.
        let expr = SymbolicExpression::list(vec![
            atom("get"),
            atom("missing"),
            SymbolicExpression::list(vec![atom("tuple"), pair("a", Value::UInt(1))]),
        ]);
        let _ = assert_paths_agree(&expr, false).unwrap_err();

        // duplicate field names error identically at construction.
        let expr = SymbolicExpression::list(vec![
            atom("tuple"),
            pair("a", Value::UInt(1)),
            pair("a", Value::UInt(2)),
        ]);
        let _ = assert_paths_agree(&expr, false).unwrap_err();
    }
}

#[cfg(test)]
mod budget_sweep_test {
    use super::test::assert_paths_agree_at_budget;
    use super::tier1_test_exprs::battery;

    /// Order-sensitive parity: both evaluators must agree at EVERY sampled
    /// runtime budget — a divergent cost-charge sequence exhausts at a
    /// different point and produces a different outcome/error/event set.
    /// (Definitive corpus-scale check remains the mainnet replay gate.)
    #[test]
    fn charge_order_parity_via_budget_sweep() {
        for (expr, read_only) in battery() {
            // Low budgets exhaustively, then coarse samples up to "plenty".
            let mut budgets: Vec<u64> = (0..=64).collect();
            budgets.extend((7..=20).map(|i| i * 25));
            budgets.push(u64::MAX);
            for b in budgets {
                let _ = assert_paths_agree_at_budget(&expr, read_only, Some(b));
            }
        }
    }
}

#[cfg(test)]
mod tier1_test_exprs {
    use crate::vm::representations::SymbolicExpression;
    use crate::vm::{ClarityName, Value};

    fn atom(s: &'static str) -> SymbolicExpression {
        SymbolicExpression::atom(ClarityName::from_literal(s))
    }
    fn uint(n: u128) -> SymbolicExpression {
        SymbolicExpression::atom_value(Value::UInt(n))
    }
    fn list(v: Vec<SymbolicExpression>) -> SymbolicExpression {
        SymbolicExpression::list(v)
    }

    /// Typed forms, error paths, and typed↔Opaque nesting in one battery.
    pub fn battery() -> Vec<(SymbolicExpression, bool)> {
        vec![
            (
                list(vec![
                    atom("if"),
                    SymbolicExpression::atom_value(Value::Bool(true)),
                    uint(1),
                    uint(2),
                ]),
                false,
            ),
            (
                list(vec![
                    atom("let"),
                    list(vec![
                        list(vec![atom("a"), uint(1)]),
                        list(vec![atom("b"), uint(2)]),
                    ]),
                    list(vec![atom("+"), atom("a"), atom("b")]),
                ]),
                false,
            ),
            (
                list(vec![
                    atom("asserts!"),
                    SymbolicExpression::atom_value(Value::Bool(false)),
                    uint(9),
                ]),
                false,
            ),
            (list(vec![atom("print"), uint(5)]), false),
            (
                list(vec![
                    atom("get"),
                    atom("a"),
                    list(vec![atom("tuple"), list(vec![atom("a"), uint(1)])]),
                ]),
                false,
            ),
            (list(vec![atom("var-get"), atom("cursor")]), false),
            (list(vec![atom("var-set"), atom("cursor"), uint(5)]), false),
            (list(vec![atom("var-set"), atom("cursor"), uint(5)]), true),
            // Typed `if` wrapping an Opaque `match` subtree.
            (
                list(vec![
                    atom("if"),
                    SymbolicExpression::atom_value(Value::Bool(true)),
                    list(vec![
                        atom("match"),
                        list(vec![atom("some"), uint(1)]),
                        atom("x"),
                        atom("x"),
                        uint(0),
                    ]),
                    uint(7),
                ]),
                false,
            ),
        ]
    }
}
