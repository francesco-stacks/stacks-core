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

const DEFAULT_FLAGS: u64 =
    flags::CAPTURE_COSTS | flags::CAPTURE_COST_FN | flags::CAPTURE_CONTRACT_CALL_IDENT;

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

/// Begin a profiler span for a Clarity built-in function.
///
/// Caches [`stacks_profiler::SpanId`] instances per `clarity_name` in a thread-local
/// to avoid repeated allocation (the standard `span!` macro uses a per-callsite
/// `OnceLock`, but here we dispatch ~80 different Clarity functions from one callsite).
///
/// Returns `Some(ProfileGuard)` when profiling is active, `None` otherwise.
/// The span's `name` is set to `clarity_name` (the Clarity-facing name such as `"+"`
/// or `"tuple"`) and its optional `tag` to `rust_name` (the backing Rust function
/// name such as `"native_add"` or `"special_tuple"`).
#[cfg(feature = "profiler")]
#[track_caller]
pub fn begin_builtin_span(
    clarity_name: &'static str,
    rust_name: &'static str,
) -> Option<stacks_profiler::ProfileGuard> {
    use std::cell::RefCell;

    if !capture_costs()
        || !stacks_profiler::Profiler::is_record_enabled()
        || stacks_profiler::Profiler::is_suppressed()
    {
        return None;
    }

    let caller = std::panic::Location::caller();

    // Thread-local cache: maps clarity_name pointer to &'static SpanId.
    // At most ~80 entries (one per Clarity built-in); linear scan is fast.
    thread_local! {
        static CACHE: RefCell<Vec<(*const u8, &'static stacks_profiler::SpanId)>> =
            const { RefCell::new(Vec::new()) };
    }

    let span_id = CACHE.with(|cache| {
        let mut vec = cache.borrow_mut();
        // Pointer equality is sound because clarity_name always originates
        // from the same &'static str returned by NativeFunctions::get_name_str().
        let ptr = clarity_name.as_ptr();
        for &(cached_ptr, id) in vec.iter() {
            if cached_ptr == ptr {
                return id;
            }
        }
        // First time seeing this clarity_name on this thread — allocate a SpanId.
        let id: &'static stacks_profiler::SpanId = Box::leak(Box::new(stacks_profiler::SpanId {
            name: clarity_name,
            context: Some("clarity::vm"),
            file: caller.file(),
            line: caller.line(),
        }));
        vec.push((ptr, id));
        id
    });

    let tag = stacks_profiler::Tag::from(rust_name);
    Some(stacks_profiler::Profiler::begin_span(span_id, Some(tag)))
}

/// Begin a profiler span for a user-defined Clarity function.
///
/// Unlike `begin_builtin_span`, user function names are dynamic (coming from
/// contracts). We cache SpanId per unique function name string, bounded by the
/// number of unique user functions called during profiling.
///
/// The span name is the Clarity function name (e.g. `"transfer-tokens"`),
/// and the tag is the fully-qualified identifier (e.g.
/// `"ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VG:my-contract:transfer-tokens"`).
/// The `define_type` (`"private"`, `"public"`, or `"read-only"`) is recorded as
/// context so the UI can visually distinguish them.
#[cfg(feature = "profiler")]
#[track_caller]
pub fn begin_user_fn_span(
    fn_name: &str,
    define_type: &crate::vm::callables::DefineType,
    full_identifier: String,
) -> Option<stacks_profiler::ProfileGuard> {
    use std::cell::RefCell;
    use std::collections::HashMap;

    if !capture_costs()
        || !stacks_profiler::Profiler::is_record_enabled()
        || stacks_profiler::Profiler::is_suppressed()
    {
        return None;
    }

    let caller = std::panic::Location::caller();

    // Thread-local cache: maps function name to &'static SpanId.
    // Bounded by the number of unique user-defined functions seen on this thread.
    thread_local! {
        static CACHE: RefCell<HashMap<String, &'static stacks_profiler::SpanId>> =
            RefCell::new(HashMap::new());
    }

    let span_id = CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        if let Some(&id) = map.get(fn_name) {
            return id;
        }
        // Leak the name string so SpanId can hold a &'static str.
        let leaked_name: &'static str = Box::leak(fn_name.to_owned().into_boxed_str());
        let context = match define_type {
            crate::vm::callables::DefineType::Public => "clarity::user::public",
            crate::vm::callables::DefineType::Private => "clarity::user::private",
            crate::vm::callables::DefineType::ReadOnly => "clarity::user::read-only",
        };
        let id: &'static stacks_profiler::SpanId = Box::leak(Box::new(stacks_profiler::SpanId {
            name: leaked_name,
            context: Some(context),
            file: caller.file(),
            line: caller.line(),
        }));
        map.insert(fn_name.to_owned(), id);
        id
    });

    let tag = stacks_profiler::Tag::from(full_identifier);
    Some(stacks_profiler::Profiler::begin_span(span_id, Some(tag)))
}

/// No-op when the `profiler` feature is not enabled.
#[cfg(not(feature = "profiler"))]
#[inline(always)]
pub fn begin_builtin_span(_clarity_name: &'static str, _rust_name: &'static str) -> Option<()> {
    None
}

/// No-op when the `profiler` feature is not enabled.
#[cfg(not(feature = "profiler"))]
#[inline(always)]
pub fn begin_user_fn_span(
    _fn_name: &str,
    _define_type: &crate::vm::callables::DefineType,
    _full_identifier: String,
) -> Option<()> {
    None
}

/// Begin a profiler span for a cross-contract call (`contract-call?`).
///
/// Emits `clarity:contract-call` with the target `contract.function` as tag.
/// Uses a static [`OnceLock`]-backed [`SpanId`](stacks_profiler::SpanId) since
/// the span name is fixed.
#[cfg(feature = "profiler")]
#[track_caller]
pub fn begin_contract_call_span(
    contract_identifier: &crate::vm::types::QualifiedContractIdentifier,
    tx_name: &str,
) -> Option<stacks_profiler::ProfileGuard> {
    use std::sync::OnceLock;

    if !capture_costs()
        || !stacks_profiler::Profiler::is_record_enabled()
        || stacks_profiler::Profiler::is_suppressed()
    {
        return None;
    }

    let caller = std::panic::Location::caller();

    static SPAN: OnceLock<stacks_profiler::SpanId> = OnceLock::new();
    let span_id = SPAN.get_or_init(|| stacks_profiler::SpanId {
        name: "clarity:contract-call?",
        context: Some("clarity::vm"),
        file: caller.file(),
        line: caller.line(),
    });

    let tag = if capture_contract_call_ident() {
        Some(stacks_profiler::Tag::from(format!(
            "{}.{}",
            contract_identifier, tx_name
        )))
    } else {
        None
    };
    Some(stacks_profiler::Profiler::begin_span(span_id, tag))
}

/// No-op when the `profiler` feature is not enabled.
#[cfg(not(feature = "profiler"))]
#[inline(always)]
pub fn begin_contract_call_span(
    _contract_identifier: &crate::vm::types::QualifiedContractIdentifier,
    _tx_name: &str,
) -> Option<()> {
    None
}

/// Begin a profiler span for a function-as-transaction execution.
///
/// Emits `clarity:exec-read-only-tx` or `clarity:exec-public-tx` depending on
/// whether the function is read-only.  The function identifier is attached as
/// tag when [`CAPTURE_CONTRACT_CALL_IDENT`](flags::CAPTURE_CONTRACT_CALL_IDENT)
/// is enabled.
#[cfg(feature = "profiler")]
#[track_caller]
pub fn begin_exec_tx_span(
    is_read_only: bool,
    fn_identifier: &crate::vm::callables::FunctionIdentifier,
) -> Option<stacks_profiler::ProfileGuard> {
    use std::sync::OnceLock;

    if !capture_costs()
        || !stacks_profiler::Profiler::is_record_enabled()
        || stacks_profiler::Profiler::is_suppressed()
    {
        return None;
    }

    let caller = std::panic::Location::caller();

    static RO_SPAN: OnceLock<stacks_profiler::SpanId> = OnceLock::new();
    static PUB_SPAN: OnceLock<stacks_profiler::SpanId> = OnceLock::new();

    let span_id = if is_read_only {
        RO_SPAN.get_or_init(|| stacks_profiler::SpanId {
            name: "clarity:exec-read-only-tx",
            context: Some("clarity::vm"),
            file: caller.file(),
            line: caller.line(),
        })
    } else {
        PUB_SPAN.get_or_init(|| stacks_profiler::SpanId {
            name: "clarity:exec-public-tx",
            context: Some("clarity::vm"),
            file: caller.file(),
            line: caller.line(),
        })
    };

    let tag = if capture_contract_call_ident() {
        Some(stacks_profiler::Tag::from(fn_identifier.to_string()))
    } else {
        None
    };
    Some(stacks_profiler::Profiler::begin_span(span_id, tag))
}

/// No-op when the `profiler` feature is not enabled.
#[cfg(not(feature = "profiler"))]
#[inline(always)]
pub fn begin_exec_tx_span(
    _is_read_only: bool,
    _fn_identifier: &crate::vm::callables::FunctionIdentifier,
) -> Option<()> {
    None
}

/// Create a profiler span when cost capturing is enabled.
///
/// Use as: `let _span = crate::profiler::profile!("name")` or
/// `let _span = crate::profiler::profile!("name", tag_value)`.
#[macro_export]
macro_rules! profile {
    ($name:expr) => {{
        #[cfg(feature = "profiler")]
        {
            if $crate::profiler::capture_costs() && stacks_profiler::Profiler::is_record_enabled() {
                Some(stacks_profiler::span!($name))
            } else {
                None
            }
        }
        #[cfg(not(feature = "profiler"))]
        {
            Option::<()>::None
        }
    }};
    ($name:expr, $tag:expr) => {{
        #[cfg(feature = "profiler")]
        {
            if $crate::profiler::capture_costs() && stacks_profiler::Profiler::is_record_enabled() {
                if $crate::profiler::is_any_enabled(
                    $crate::profiler::flags::CAPTURE_COST_FN
                        | $crate::profiler::flags::CAPTURE_CONTRACT_CALL_IDENT,
                ) {
                    let __tag = $tag.to_string();
                    Some(stacks_profiler::span!($name, __tag))
                } else {
                    Some(stacks_profiler::span!($name))
                }
            } else {
                None
            }
        }
        #[cfg(not(feature = "profiler"))]
        {
            Option::<()>::None
        }
    }};
}

/// Record the active Clarity function name when cost capturing is enabled.
///
/// Use as: `crate::profiler::record_name!(name)` where `name` is a Clarity identifier.
#[macro_export]
macro_rules! record_name {
    ($name:expr) => {{
        #[cfg(feature = "profiler")]
        {
            if $crate::profiler::capture_costs()
                && stacks_profiler::Profiler::is_record_enabled()
                && $crate::profiler::is_any_enabled(
                    $crate::profiler::flags::CAPTURE_COST_FN
                        | $crate::profiler::flags::CAPTURE_CONTRACT_CALL_IDENT,
                )
            {
                stacks_profiler::record!("NAME", $name.to_string());
            }
        }
        #[cfg(not(feature = "profiler"))]
        {
            let _ = &$name;
        }
    }};
}

#[doc(hidden)]
pub use crate::profile;
#[doc(hidden)]
pub use crate::record_name;
