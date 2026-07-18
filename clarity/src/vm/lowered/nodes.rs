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

use std::collections::HashMap;
use std::sync::Arc;

use crate::vm::database::DataVariableMetadata;
use crate::vm::functions::NativeFunctions;
use crate::vm::representations::{ClarityName, SymbolicExpression};
use crate::vm::types::Value;

/// All lowered function bodies of one contract; stored (never serialized)
/// on `ContractContext` and shared via `Arc` with the contract cache.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredContract {
    pub functions: HashMap<ClarityName, LExpr>,
}

/// A lowered expression. Laziness is structural: lazy operands are `LExpr`
/// fields evaluated inside the form's typed core; eager calls hold argument
/// vectors evaluated by the shared eager-argument loop.
#[derive(Debug, Clone, PartialEq)]
pub enum LExpr {
    /// `AtomValue` / `LiteralValue`.
    Literal(Value),
    /// Atom. Resolved at runtime through the existing `lookup_variable`
    /// (keeps reserved-variable handling, `LookupVariableDepth` charges, and
    /// `ValueRef` borrowing semantics).
    Var(ClarityName),
    /// A function application whose shape was proven at lower time.
    Call(Box<LCall>),
    /// Anything lowering could not prove well-formed. Evaluated by the legacy
    /// `eval` on the retained original expression, subtree included.
    Opaque(SymbolicExpression),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LCall {
    pub kind: LCallKind,
}

/// Phase-0 subset of call shapes; the full design grows this to ~40 variants.
#[derive(Debug, Clone, PartialEq)]
// Variant sizes differ, but LCall is always Box'd inside LExpr::Call: inline
// fields share that one heap node (one allocation per call, better locality).
#[allow(clippy::large_enum_variant)]
pub enum LCallKind {
    /// The ~90 eager natives: evaluate all args, then dispatch through the
    /// existing `NativeHandle` machinery.
    Native {
        func: NativeFunctions,
        args: Vec<LExpr>,
    },
    /// Call to a function defined in the same contract.
    User {
        name: ClarityName,
        args: Vec<LExpr>,
    },
    If {
        cond: LExpr,
        then_expr: LExpr,
        else_expr: LExpr,
    },
    Let {
        bindings: Vec<(ClarityName, LExpr)>,
        body: Vec<LExpr>,
    },
    Asserts {
        cond: LExpr,
        thrown: LExpr,
    },
    Print {
        value: LExpr,
    },
    TupleGet {
        field: ClarityName,
        tuple: LExpr,
    },
    TupleCons {
        bindings: Vec<(ClarityName, LExpr)>,
    },
    VarGet {
        var: ClarityName,
        meta: Arc<DataVariableMetadata>,
    },
    VarSet {
        var: ClarityName,
        meta: Arc<DataVariableMetadata>,
        value: LExpr,
    },
}

impl LExpr {
    /// Depth-first visit of this node and every lowered descendant.
    /// `Opaque` payloads are leaves here: their subtrees were never lowered.
    pub fn walk(&self, visit: &mut impl FnMut(&LExpr)) {
        visit(self);
        if let LExpr::Call(call) = self {
            match &call.kind {
                LCallKind::Native { args, .. } | LCallKind::User { args, .. } => {
                    for arg in args {
                        arg.walk(visit);
                    }
                }
                LCallKind::If {
                    cond,
                    then_expr,
                    else_expr,
                } => {
                    cond.walk(visit);
                    then_expr.walk(visit);
                    else_expr.walk(visit);
                }
                LCallKind::Let { bindings, body } => {
                    for (_, value) in bindings {
                        value.walk(visit);
                    }
                    for body_expr in body {
                        body_expr.walk(visit);
                    }
                }
                LCallKind::Asserts { cond, thrown } => {
                    cond.walk(visit);
                    thrown.walk(visit);
                }
                LCallKind::Print { value } => value.walk(visit),
                LCallKind::TupleGet { tuple, .. } => tuple.walk(visit),
                LCallKind::TupleCons { bindings } => {
                    for (_, value) in bindings {
                        value.walk(visit);
                    }
                }
                LCallKind::VarGet { .. } => {}
                LCallKind::VarSet { value, .. } => value.walk(visit),
            }
        }
    }
}
