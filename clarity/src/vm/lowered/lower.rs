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

//! Total lowering from `SymbolicExpression` to [`LExpr`].
//!
//! Never fails and never touches the database: shape mismatches lower the
//! WHOLE call node to [`LExpr::Opaque`] (children unlowered), so legacy
//! `eval` reproduces today's behavior (including its exact errors and the
//! cost charges on error paths) for anything unexpected.

use std::sync::Arc;

use super::nodes::{LCall, LCallKind, LExpr, LoweredContract};
use crate::vm::callables::CallableType;
use crate::vm::contexts::ContractContext;
use crate::vm::functions::{NativeFunctions, lookup_reserved_functions};
use crate::vm::representations::SymbolicExpression;
use crate::vm::representations::SymbolicExpressionType::{
    Atom, AtomValue, Field, List, LiteralValue, TraitReference,
};

/// Lower every function body in a (fully initialized, post-canonicalize)
/// contract. Total: bodies that don't fit the typed subset become `Opaque`.
pub fn lower_contract(contract_context: &ContractContext) -> LoweredContract {
    LoweredContract {
        functions: contract_context
            .functions
            .iter()
            .map(|(name, function)| {
                (
                    name.clone(),
                    lower_function_body(function.get_body(), contract_context),
                )
            })
            .collect(),
    }
}

/// Lower one function body. Pure in the contract context and its
/// `ClarityVersion`; the Phase-0 subset parses no type annotations, so the
/// epoch does not yet participate (it will when `from-consensus-buff?` and
/// friends get typed variants).
pub fn lower_function_body(body: &SymbolicExpression, contract_context: &ContractContext) -> LExpr {
    lower_expr(body, contract_context)
}

fn lower_expr(expr: &SymbolicExpression, contract_context: &ContractContext) -> LExpr {
    match &expr.expr {
        AtomValue(value) | LiteralValue(value) => LExpr::Literal(value.clone()),
        Atom(name) => LExpr::Var(name.clone()),
        List(children) => lower_call(expr, children, contract_context),
        // Trait/field references only appear in positions the legacy
        // evaluator handles specially; keep them opaque.
        TraitReference(..) | Field(..) => LExpr::Opaque(expr.clone()),
    }
}

fn lower_call(
    expr: &SymbolicExpression,
    children: &[SymbolicExpression],
    contract_context: &ContractContext,
) -> LExpr {
    let opaque = || LExpr::Opaque(expr.clone());

    let Some((head, args)) = children.split_first() else {
        return opaque();
    };
    let Some(name) = head.match_atom() else {
        return opaque();
    };
    let version = contract_context.get_clarity_version();

    // Mirror lookup_function's precedence: reserved names shadow user
    // functions (vm/mod.rs lookup_function).
    let kind = match lookup_reserved_functions(name, version) {
        Some(CallableType::NativeFunction(..)) | Some(CallableType::NativeFunction205(..)) => {
            // Eager natives: arity is checked at runtime by NativeHandle
            // (reachable via fold/map/filter callbacks), so no shape check
            // here — all children lower recursively.
            // Same (name, version) that lookup_reserved_functions resolved,
            // so this is infallible in practice — but lowering must be total,
            // so an inconsistency degrades to Opaque instead of panicking.
            let Some(func) = NativeFunctions::lookup_by_name_at_version(name, version) else {
                return opaque();
            };
            LCallKind::Native {
                func,
                args: args
                    .iter()
                    .map(|a| lower_expr(a, contract_context))
                    .collect(),
            }
        }
        Some(CallableType::SpecialFunction(..)) => {
            let Some(func) = NativeFunctions::lookup_by_name_at_version(name, version) else {
                return opaque();
            };
            match lower_special(func, args, contract_context) {
                Some(kind) => kind,
                None => return opaque(),
            }
        }
        Some(CallableType::UserFunction(..)) => return opaque(),
        None => {
            if contract_context.functions.contains_key(name) {
                LCallKind::User {
                    name: name.clone(),
                    args: args
                        .iter()
                        .map(|a| lower_expr(a, contract_context))
                        .collect(),
                }
            } else {
                // Undefined function: legacy eval produces the canonical
                // UndefinedFunction error at runtime.
                return opaque();
            }
        }
    };

    LExpr::Call(Box::new(LCall { kind }))
}

/// Phase-0 special forms. Returns `None` (→ Opaque) for any special form
/// outside the subset or any shape the legacy parser would reject.
fn lower_special(
    func: NativeFunctions,
    args: &[SymbolicExpression],
    contract_context: &ContractContext,
) -> Option<LCallKind> {
    let lower = |a: &SymbolicExpression| lower_expr(a, contract_context);
    match func {
        NativeFunctions::If => {
            let [cond, then_expr, else_expr] = args else {
                return None;
            };
            Some(LCallKind::If {
                cond: lower(cond),
                then_expr: lower(then_expr),
                else_expr: lower(else_expr),
            })
        }
        NativeFunctions::Let => {
            let (binding_list, body) = args.split_first()?;
            if body.is_empty() {
                return None;
            }
            let mut bindings = Vec::new();
            for binding in binding_list.match_list()? {
                let [name_expr, value_expr] = binding.match_list()? else {
                    return None;
                };
                bindings.push((name_expr.match_atom()?.clone(), lower(value_expr)));
            }
            Some(LCallKind::Let {
                bindings,
                body: body.iter().map(lower).collect(),
            })
        }
        NativeFunctions::Asserts => {
            let [cond, thrown] = args else {
                return None;
            };
            Some(LCallKind::Asserts {
                cond: lower(cond),
                thrown: lower(thrown),
            })
        }
        // Legacy print reads args.first() without an arity check; only the
        // analysis-legal single-argument shape is typed, the rest stays
        // Opaque (where legacy behaves identically anyway).
        NativeFunctions::Print => {
            let [value] = args else {
                return None;
            };
            Some(LCallKind::Print {
                value: lower(value),
            })
        }
        NativeFunctions::TupleGet => {
            let [name_expr, tuple_expr] = args else {
                return None;
            };
            Some(LCallKind::TupleGet {
                field: name_expr.match_atom()?.clone(),
                tuple: lower(tuple_expr),
            })
        }
        // Unlike `let`, every argument of `tuple` is itself one binding pair.
        NativeFunctions::TupleCons => {
            if args.is_empty() {
                return None;
            }
            let mut bindings = Vec::new();
            for binding in args {
                let [name_expr, value_expr] = binding.match_list()? else {
                    return None;
                };
                bindings.push((name_expr.match_atom()?.clone(), lower(value_expr)));
            }
            Some(LCallKind::TupleCons { bindings })
        }
        NativeFunctions::FetchVar => {
            let [name_expr] = args else {
                return None;
            };
            let var = name_expr.match_atom()?;
            let meta = contract_context.meta_data_var.get(var)?;
            Some(LCallKind::VarGet {
                var: var.clone(),
                meta: Arc::new(meta.clone()),
            })
        }
        NativeFunctions::SetVar => {
            let [name_expr, value_expr] = args else {
                return None;
            };
            let var = name_expr.match_atom()?;
            let meta = contract_context.meta_data_var.get(var)?;
            Some(LCallKind::VarSet {
                var: var.clone(),
                meta: Arc::new(meta.clone()),
                value: lower(value_expr),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::vm::database::DataVariableMetadata;
    use crate::vm::types::{QualifiedContractIdentifier, TypeSignature, Value};
    use crate::vm::{ClarityName, ClarityVersion};

    fn ctx() -> ContractContext {
        let mut c = ContractContext::new(
            QualifiedContractIdentifier::transient(),
            ClarityVersion::latest(),
        );
        c.meta_data_var.insert(
            ClarityName::from_literal("cursor"),
            DataVariableMetadata {
                value_type: TypeSignature::UIntType,
            },
        );
        c
    }

    fn atom(s: &'static str) -> SymbolicExpression {
        SymbolicExpression::atom(ClarityName::from_literal(s))
    }

    fn list(v: Vec<SymbolicExpression>) -> SymbolicExpression {
        SymbolicExpression::list(v)
    }

    #[test]
    fn lowers_subset_shapes() {
        let c = ctx();
        // (begin (var-get cursor))
        let body = list(vec![
            atom("begin"),
            list(vec![atom("var-get"), atom("cursor")]),
        ]);
        let LExpr::Call(call) = lower_function_body(&body, &c) else {
            panic!("expected Call");
        };
        let LCallKind::Native {
            func: NativeFunctions::Begin,
            args,
        } = call.kind
        else {
            panic!("expected eager-native begin");
        };
        assert!(matches!(
            &args[0],
            LExpr::Call(c) if matches!(&c.kind, LCallKind::VarGet { var, .. } if var.as_str() == "cursor")
        ));

        // (+ u1 u2) → eager native with literal args
        let body = list(vec![
            atom("+"),
            SymbolicExpression::atom_value(Value::UInt(1)),
            SymbolicExpression::atom_value(Value::UInt(2)),
        ]);
        let LExpr::Call(call) = lower_function_body(&body, &c) else {
            panic!("expected Call");
        };
        assert!(matches!(
            call.kind,
            LCallKind::Native { func: NativeFunctions::Add, ref args } if args.len() == 2
        ));
    }

    #[test]
    fn shape_mismatches_lower_to_opaque() {
        let c = ctx();
        for body in [
            list(vec![atom("if"), atom("cursor")]),         // bad if arity
            list(vec![atom("var-get"), atom("missing")]),   // no such var
            list(vec![atom("var-get")]),                    // bad arity
            list(vec![atom("no-such-fn"), atom("cursor")]), // undefined fn
            list(vec![atom("match"), atom("cursor")]),      // outside subset
            list(vec![]),                                   // empty application
            list(vec![atom("asserts!"), atom("cursor")]),   // bad asserts! arity
            list(vec![atom("print")]),                      // print with no args
            list(vec![atom("print"), atom("cursor"), atom("cursor")]), // extra print args
            list(vec![atom("get"), list(vec![]), atom("cursor")]), // non-atom field
            list(vec![atom("tuple")]),                      // empty tuple
            list(vec![atom("tuple"), atom("cursor")]),      // non-pair binding
        ] {
            assert!(
                matches!(lower_function_body(&body, &c), LExpr::Opaque(_)),
                "expected Opaque for {body:?}"
            );
        }
    }
}

#[cfg(test)]
mod bench {
    use std::time::Instant;

    use stacks_common::consts::CHAIN_ID_TESTNET;
    use stacks_common::types::StacksEpochId;

    use super::*;
    use crate::vm::costs::LimitedCostTracker;
    use crate::vm::database::MemoryBackingStore;
    use crate::vm::lowered::eval_lowered;
    use crate::vm::types::{QualifiedContractIdentifier, Value};
    use crate::vm::{
        CallStack, ClarityVersion, ContractContext, ExecutionState, GlobalContext,
        InvocationContext, LocalContext, eval,
    };

    /// Build a balanced arithmetic tree of the given depth:
    /// (+ (+ u1 u1) (+ u1 u1)) ... with an `if` at the root.
    fn arith_tree(depth: u32) -> SymbolicExpression {
        fn node(depth: u32) -> SymbolicExpression {
            if depth == 0 {
                SymbolicExpression::atom_value(Value::UInt(1))
            } else {
                SymbolicExpression::list(vec![
                    SymbolicExpression::atom(ClarityName::from_literal("+")),
                    node(depth - 1),
                    node(depth - 1),
                ])
            }
        }
        SymbolicExpression::list(vec![
            SymbolicExpression::atom(ClarityName::from_literal("if")),
            SymbolicExpression::atom_value(Value::Bool(true)),
            node(depth),
            SymbolicExpression::atom_value(Value::UInt(0)),
        ])
    }

    use crate::vm::ClarityName;

    /// Not a correctness test: prints ns/iter for legacy eval vs eval_lowered
    /// on the same body. Run with:
    /// `cargo test -p clarity --release --lib lowered::lower::bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn phase0_interpreter_microbench() {
        const ITERS: u32 = 20_000;
        let body = arith_tree(8); // 2^8 leaf literals, 255 native adds
        let contract_context = ContractContext::new(
            QualifiedContractIdentifier::transient(),
            ClarityVersion::latest(),
        );
        let lowered = lower_function_body(&body, &contract_context);
        assert!(!matches!(lowered, LExpr::Opaque(_)));

        let mut marf = MemoryBackingStore::new();
        let mut global_context = GlobalContext::new(
            false,
            CHAIN_ID_TESTNET,
            marf.as_clarity_db(),
            LimitedCostTracker::new_free(),
            StacksEpochId::latest(),
        );
        let context = LocalContext::new();
        let mut call_stack = CallStack::new();
        global_context.begin();
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

        // Warm up + verify identical results first.
        let legacy_val = eval(&body, &mut exec_state, &invoke_ctx, &context)
            .unwrap()
            .clone_with_cost(&mut exec_state)
            .unwrap();
        let lowered_val = eval_lowered(&lowered, &mut exec_state, &invoke_ctx, &context)
            .unwrap()
            .clone_with_cost(&mut exec_state)
            .unwrap();
        assert_eq!(legacy_val, lowered_val);

        let t = Instant::now();
        for _ in 0..ITERS {
            let v = eval(&body, &mut exec_state, &invoke_ctx, &context).unwrap();
            std::hint::black_box(v);
        }
        let legacy_ns = t.elapsed().as_nanos() / ITERS as u128;

        let t = Instant::now();
        for _ in 0..ITERS {
            let v = eval_lowered(&lowered, &mut exec_state, &invoke_ctx, &context).unwrap();
            std::hint::black_box(v);
        }
        let lowered_ns = t.elapsed().as_nanos() / ITERS as u128;

        let t = Instant::now();
        for _ in 0..ITERS {
            let l = lower_function_body(&body, &contract_context);
            std::hint::black_box(l);
        }
        let lowering_ns = t.elapsed().as_nanos() / ITERS as u128;

        println!("legacy eval:    {legacy_ns} ns/iter");
        println!("eval_lowered:   {lowered_ns} ns/iter");
        println!("lowering pass:  {lowering_ns} ns/iter (one-time per load in Phase 1)");
        println!(
            "speedup: {:.2}x",
            legacy_ns as f64 / lowered_ns.max(1) as f64
        );
    }
}
