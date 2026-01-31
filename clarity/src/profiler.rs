// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
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

//! Runtime profiler configuration for Clarity instrumentation.
//!
//! This module is feature-gated behind `profiler` and provides low-overhead
//! toggles for profiler capture modes.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bitflags that control profiler capture behavior.
pub mod flags {
    /// Capture per-call execution costs (counters in `runtime_cost`).
    pub const CAPTURE_COSTS: u64 = 1 << 0;
    /// Attach cost function identifiers as span tags.
    pub const CAPTURE_COST_FN: u64 = 1 << 1;
    /// Attach contract-call function identifiers as span tags.
    pub const CAPTURE_CONTRACT_CALL_IDENT: u64 = 1 << 2;
}

const DEFAULT_FLAGS: u64 = flags::CAPTURE_COSTS | flags::CAPTURE_COST_FN;

static PROFILER_FLAGS: AtomicU64 = AtomicU64::new(DEFAULT_FLAGS);

/// Replace the current profiler flags and return the previous value.
#[inline(always)]
pub fn set_flags(flags: u64) -> u64 {
    PROFILER_FLAGS.swap(flags, Ordering::Relaxed)
}

/// Enable the given flags.
#[inline(always)]
pub fn enable(flags: u64) {
    PROFILER_FLAGS.fetch_or(flags, Ordering::Relaxed);
}

/// Disable the given flags.
#[inline(always)]
pub fn disable(flags: u64) {
    PROFILER_FLAGS.fetch_and(!flags, Ordering::Relaxed);
}

/// Returns true if all given flags are enabled.
#[inline(always)]
pub fn is_enabled(flags: u64) -> bool {
    (PROFILER_FLAGS.load(Ordering::Relaxed) & flags) == flags
}

/// Returns true if any given flags are enabled.
#[inline(always)]
pub fn is_any_enabled(flags: u64) -> bool {
    (PROFILER_FLAGS.load(Ordering::Relaxed) & flags) != 0
}

#[inline(always)]
pub fn capture_costs() -> bool {
    is_enabled(flags::CAPTURE_COSTS)
}

#[inline(always)]
pub fn capture_contract_call_ident() -> bool {
    is_enabled(flags::CAPTURE_CONTRACT_CALL_IDENT)
}
