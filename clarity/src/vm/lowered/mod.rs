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

//! Typed lowered IR for Clarity function bodies.
//!
//! `LExpr` is an in-memory representation built from a contract's
//! `SymbolicExpression` bodies at load time. It is NEVER persisted — the
//! stored contract format is untouched — and lowering is a pure function of
//! (post-canonicalize `ContractContext`, epoch, `ClarityVersion`).
//!
//! Consensus-safety pillars:
//! 1. Lowering is TOTAL: any expression that does not match an expected shape
//!    lowers to [`LExpr::Opaque`], which the evaluator executes through the
//!    unchanged legacy `eval` — so unexpected SHAPES never run new code.
//!    (Runtime-invalid values inside typed shapes error in the typed cores,
//!    which replicate the legacy errors exactly.)
//! 2. The typed evaluator replays the legacy cost/memory sequence exactly
//!    (see `eval.rs` parity contract; the gate is suite-wide parity plus mainnet replay).
//!
//! Typed node subset: literals, variable refs, eager native calls, user
//! calls, `if`, `let`, `asserts!`, `print`, `get`, `tuple`, `var-get`,
//! `var-set` (`begin` is an eager native). Everything else lowers to
//! `Opaque`.

mod eval;
mod lower;
mod nodes;

pub use eval::eval_lowered;
pub use lower::{lower_contract, lower_function_body};
pub use nodes::{LCall, LCallKind, LExpr, LoweredContract};
