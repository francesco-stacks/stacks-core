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

/// Execute a block of code inside a profiling span.
///
/// This is the most convenient way to profile a scope because it guarantees the span is ended even
/// if the block returns early or panics (the guard is dropped).
///
/// `measure!` is implemented in terms of [`span!`](crate::span), and uses the same sampling and
/// tagging options.
///
/// ## When to use
/// - You want to profile a lexical scope (a block).
/// - You don't need to manually keep the guard around.
/// - You want panic/early-return safety without writing `let _guard = ...`.
///
/// If you need a guard whose lifetime is not exactly the block scope, use [`span!`](crate::span)
/// directly.
///
/// ## Sampling
/// The `rate: N` forms record timing for approximately **1 out of every N executions** at the
/// callsite. Lower `N` means more overhead and better fidelity.
///
/// By default, if a span is **not sampled**, `span!` returns `None` (no guard is created), which is
/// the cheapest path.
///
/// ## Suppression vs count-only (for unsampled calls)
/// Two optional modes affect what happens on the **unsampled** path:
///
/// - `suppress`: unsampled parent spans enter *hierarchical suppression*. While suppressed, nested
///   `span!`/`measure!` calls become no-ops so that children do **not** incorrectly attach to the
///   nearest sampled ancestor. This prevents tree distortion but drops nested detail under
///   unsampled parents.
///
/// - `count_only`: unsampled parent spans still push a lightweight frame to preserve hierarchy and
///   increment counts, but do **not** read clocks. This keeps children correctly parented and
///   yields accurate per-context call counts, at higher overhead than `suppress` / returning
///   `None`.
///
/// ## Tagging
/// Many forms accept a `$tag` which is converted into [`Tag`](crate::Tag). Tags are used to
/// distinguish otherwise identical spans (for example, different transaction indices).
///
/// Be careful with very high-cardinality tags (e.g. unique IDs) at hot callsites: this can create
/// many distinct nodes in the profile tree.
///
/// ## Examples
///
/// ### Basic (always recorded)
/// ```rust
/// use stacks_profiler::measure;
///
/// measure!("decode_block", {
///     // ...work...
/// });
/// ```
///
/// ### With a tag
/// ```rust
/// use stacks_profiler::measure;
///
/// let tx_index: u64 = 42;
/// measure!("execute_tx", tx_index, {
///     // ...work...
/// });
/// ```
///
/// ### Sample 1/N (unsampled => nothing recorded)
/// ```rust
/// use stacks_profiler::measure;
///
/// // Time ~1% of calls at this callsite.
/// measure!("hot_loop_body", rate: 100, {
///     // ...work...
/// });
/// ```
///
/// ### Sample 1/N with hierarchical suppression
/// ```rust
/// use stacks_profiler::measure;
///
/// // If unsampled, enter suppression so nested spans don't attach to the wrong parent.
/// measure!("tx", rate: 100, suppress, {
///     // Nested spans are only recorded when `tx` itself is sampled.
///     measure!("verify", { /* ... */ });
///     measure!("apply",  { /* ... */ });
/// });
/// ```
///
/// ### Sample 1/N with count-only frames
/// ```rust
/// use stacks_profiler::measure;
///
/// // If unsampled, keep the hierarchy and increment counts without timing.
/// measure!("tx", rate: 100, count_only, {
///     // Nested spans will still attach under `tx` even when `tx` is not timed.
///     measure!("verify", { /* ... */ });
///     measure!("apply",  { /* ... */ });
/// });
/// ```
///
/// ## Notes
/// - All macros are thread-local: each thread records its own tree.
/// - `count_only` affects how you should interpret `count`: `count` becomes "number of calls"
///   (sampled + unsampled), while timing fields are accumulated only for sampled calls.
#[macro_export]
macro_rules! measure {
    // Name, Tag, Rate, count_only, Block
    ($name:literal, $tag:expr, rate: $rate:literal, count_only, $block:block) => {{
        let _guard = $crate::span!($name, $tag, rate: $rate, count_only);
        $block
    }};

    // Name, Rate, count_only, Block
    ($name:literal, rate: $rate:literal, count_only, $block:block) => {{
        let _guard = $crate::span!($name, rate: $rate, count_only);
        $block
    }};

    // Name, Tag, Rate, suppress, Block
    ($name:literal, $tag:expr, rate: $rate:literal, suppress, $block:block) => {{
        let _guard = $crate::span!($name, $tag, rate: $rate, suppress);
        $block
    }};

    // Name, Rate, suppress, Block
    ($name:literal, rate: $rate:literal, suppress, $block:block) => {{
        let _guard = $crate::span!($name, rate: $rate, suppress);
        $block
    }};

    // Name, Tag, Rate, Block
    ($name:literal, $tag:expr, rate: $rate:literal, $block:block) => {{
        let _guard = $crate::span!($name, $tag, rate: $rate);
        $block
    }};

    // Name, Rate, Block
    ($name:literal, rate: $rate:literal, $block:block) => {{
        let _guard = $crate::span!($name, rate: $rate);
        $block
    }};

    // Name, Tag, Block
    ($name:literal, $tag:expr, $block:block) => {{
        let _guard = $crate::span!($name, $tag);
        $block
    }};

    // Name, Block
    ($name:literal, $block:block) => {{
        let _guard = $crate::span!($name);
        $block
    }};

    // Trap (Name, Rate)
    ($name:literal, rate: $rate:literal) => {
        let _guard = $crate::span!($name, rate: $rate);
    };

    // Trap (Name)
    ($name:literal) => {
        let _guard = $crate::span!($name);
    };

    // Anonymous Block
    ($($t:tt)*) => {{
        let _guard = $crate::span!("scope");
        $($t)*
    }};
}

/// Create a profiling span guard for the current scope.
///
/// This macro returns an `Option<ProfileGuard>`:
/// - `Some(guard)` when the span is recorded (timed, count-only, or suppression guard),
/// - `None` when the span is not recorded (fast path).
///
/// Most callsites will use:
/// ```rust
/// # use stacks_profiler::span;
/// let _guard = span!("my_span");
/// ```
/// and ignore the returned value.
///
/// ## When to use
/// Use `span!` when you need:
/// - a guard that outlives a single block (e.g., around a loop where you `break`/`continue`),
/// - to manually manage the scope (`drop(_guard)` to end early),
/// - to attach a tag at the callsite.
///
/// If you just want to profile a block, [`measure!`](crate::measure) is shorter and harder to misuse.
///
/// ## Supported forms
/// ### Always recorded (timed)
/// - `span!("name")`
/// - `span!("name", tag)`
///
/// ### Sampled (timed 1/N, unsampled => `None`)
/// - `span!("name", rate: N)`
/// - `span!("name", tag, rate: N)`
///
/// ### Sampled + hierarchical suppression (unsampled => suppression guard)
/// - `span!("name", rate: N, suppress)`
/// - `span!("name", tag, rate: N, suppress)`
///
/// Use this when you want to avoid *tree distortion* in analyses that depend on the true
/// hierarchical parent, and it is acceptable to lose nested detail when the parent is unsampled.
///
/// ### Sampled + count-only frames (unsampled => count-only guard)
/// - `span!("name", rate: N, count_only)`
/// - `span!("name", tag, rate: N, count_only)`
///
/// Use this when you want correct parent/child relationships *and* accurate per-context
/// call counts even when you are not timing a particular invocation.
///
/// ## Suppression behavior
/// When suppression is active (entered via an unsampled `suppress` span), all nested spans
/// return `None` immediately. This ensures nested spans do not attach to the wrong parent.
///
/// ## Count-only semantics
/// In `count_only` mode, the profiler:
/// - maintains parent context (push/pop a lightweight frame),
/// - increments `count`,
/// - **does not** read wall/cpu clocks for that invocation.
///
/// As a result:
/// - `count` becomes total calls (sampled + unsampled),
/// - `wall_time_ns` / `cpu_time_ns` are accumulated only for sampled calls.
///
/// ## Examples
/// ### Manual guard usage
/// ```rust
/// use stacks_profiler::span;
///
/// let guard = span!("outer");
/// // ... do some work ...
/// drop(guard); // end span early (optional)
/// ```
///
/// ### Sampling (fast unsampled path)
/// ```rust
/// use stacks_profiler::span;
///
/// // Time about 1 out of every 100 calls at this callsite.
/// let _g = span!("hot", rate: 100);
/// ```
///
/// ### Avoid wrong-parent attachment
/// ```rust
/// use stacks_profiler::{measure, span};
///
/// measure!("root", {
///     let _p = span!("parent", rate: 100, suppress);
///     // This child will only be recorded when `parent` is sampled.
///     let _c = span!("child");
/// });
/// ```
///
/// ### Preserve hierarchy with count-only
/// ```rust
/// use stacks_profiler::{measure, span};
///
/// measure!("root", {
///     let _p = span!("parent", rate: 100, count_only);
///     // This child will attach under `parent` even when `parent` is not timed.
///     let _c = span!("child");
/// });
/// ```
///
/// ## Performance notes
/// - `rate: N` unsampled (default) is the cheapest path.
/// - `suppress` unsampled adds a very small overhead (TLS suppression depth).
/// - `count_only` unsampled is more expensive because it must preserve hierarchy and update counts.
#[macro_export]
macro_rules! span {
    // Internal helpers

    (@get_id $name:literal) => {{
        static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
        __PROFILER_SPAN_ID.get_or_init(|| $crate::Profiler::new_span_id($name).with_context(module_path!()))
    }};

    (@begin $id:expr, $tag_opt:expr) => {{
        Some($crate::Profiler::begin_span($id, $tag_opt))
    }};

    (@should_sample $counter:ident, $rate:literal) => {{
        const __RATE: usize = $rate;
        if __RATE <= 1 {
            true
        } else {
            let __n = $counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if __RATE.is_power_of_two() {
                (__n & (__RATE - 1)) == 0
            } else {
                (__n % __RATE) == 0
            }
        }
    }};

    // Public forms

    // Name, Tag, Rate, count_only
    ($name:literal, $tag:expr, rate: $rate:literal, count_only) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            // Hoist id + tag so both branches share the same OnceLock/static.
            let __id = $crate::span!(@get_id $name);
            let __tag: $crate::Tag = ::core::convert::Into::into($tag);

            if $crate::span!(@should_sample __PROFILER_SAMPLE_COUNTER, $rate) {
                $crate::span!(@begin __id, Some(__tag))
            } else {
                Some($crate::Profiler::begin_span_count_only(__id, Some(__tag)))
            }
        }
    }};

    // Name, Rate, count_only
    ($name:literal, rate: $rate:literal, count_only) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            // Hoist id so both branches share the same OnceLock/static.
            let __id = $crate::span!(@get_id $name);

            if $crate::span!(@should_sample __PROFILER_SAMPLE_COUNTER, $rate) {
                $crate::span!(@begin __id, None)
            } else {
                Some($crate::Profiler::begin_span_count_only(__id, None))
            }
        }
    }};

    // Name, Tag, Rate, suppress
    ($name:literal, $tag:expr, rate: $rate:literal, suppress) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            if $crate::span!(@should_sample __PROFILER_SAMPLE_COUNTER, $rate) {
                let __id = $crate::span!(@get_id $name);
                let __tag: $crate::Tag = ::core::convert::Into::into($tag);
                $crate::span!(@begin __id, Some(__tag))
            } else {
                Some($crate::Profiler::begin_suppression())
            }
        }
    }};

    // Name, Rate, suppress
    ($name:literal, rate: $rate:literal, suppress) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            if $crate::span!(@should_sample __PROFILER_SAMPLE_COUNTER, $rate) {
                let __id = $crate::span!(@get_id $name);
                $crate::span!(@begin __id, None)
            } else {
                Some($crate::Profiler::begin_suppression())
            }
        }
    }};

    // Name, Tag, Rate (default: unsampled => None)
    ($name:literal, $tag:expr, rate: $rate:literal) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            if $crate::span!(@should_sample __PROFILER_SAMPLE_COUNTER, $rate) {
                let __id = $crate::span!(@get_id $name);
                let __tag: $crate::Tag = ::core::convert::Into::into($tag);
                $crate::span!(@begin __id, Some(__tag))
            } else {
                None
            }
        }
    }};

    // Name, Rate (default: unsampled => None)
    ($name:literal, rate: $rate:literal) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            if $crate::span!(@should_sample __PROFILER_SAMPLE_COUNTER, $rate) {
                let __id = $crate::span!(@get_id $name);
                $crate::span!(@begin __id, None)
            } else {
                None
            }
        }
    }};

    // Name, Tag
    ($name:literal, $tag:expr) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            let __id = $crate::span!(@get_id $name);
            let __tag: $crate::Tag = ::core::convert::Into::into($tag);
            $crate::span!(@begin __id, Some(__tag))
        }
    }};

    // Name
    ($name:literal) => {{
        if $crate::Profiler::is_suppressed() {
            None
        } else {
            let __id = $crate::span!(@get_id $name);
            $crate::span!(@begin __id, None)
        }
    }};
}

/// Conditionally create a profiling span guard based on a predicate.
///
/// This macro returns `None` when the predicate is false and otherwise forwards its arguments to
/// [`span!`](crate::span).
#[macro_export]
macro_rules! span_if {
    ($pred:expr, $($rest:tt)+) => {{
        if $pred {
            $crate::span!($($rest)+)
        } else {
            None
        }
    }};
}

/// Attach a key/value record to the current span (if any).
///
/// Records are stored per-occurrence (not aggregated). The value is converted via
/// `Into<RecordValue>`, which accepts `&'static str`, `u64`, `i64`, and `bool`.
///
/// ## Examples
///
/// ```rust
/// use stacks_profiler::{measure, record};
///
/// measure!("load_contract", {
///     record!("contract_id", "SP000000000000000000002Q6VF78.pox-4");
///     record!("deploy_height", 100u64);
/// });
/// ```
#[macro_export]
macro_rules! record {
    ($key:literal, $val:expr) => {{
        $crate::Profiler::record($key, ($val).into());
    }};
}

/// Attach a key/value record to the current span (if any), when a predicate is true.
///
/// Equivalent to `if pred { record!(key, val) }`, but reads more naturally at callsites that are
/// gated on a runtime flag.
///
/// ## Examples
///
/// ```rust
/// use stacks_profiler::{measure, record_if};
///
/// let verbose = true;
/// measure!("process", {
///     record_if!(verbose, "debug_info", "extra detail");
/// });
/// ```
#[macro_export]
macro_rules! record_if {
    ($pred:expr, $key:literal, $val:expr) => {{
        if $pred {
            $crate::Profiler::record($key, ($val).into());
        }
    }};
}

/// Increment a named counter on the current span (aggregated by key).
///
/// If a counter with this key already exists on the span, `delta` is added to it (saturating).
/// Otherwise a new counter is created with the given value.
///
/// ## Examples
///
/// ```rust
/// use stacks_profiler::{measure, counter_add};
///
/// measure!("process_block", {
///     for chunk in [1024u64, 2048, 512] {
///         counter_add!("bytes_read", chunk);
///     }
///     // The span will show a single counter: bytes_read = 3584
/// });
/// ```
#[macro_export]
macro_rules! counter_add {
    ($key:literal, $delta:expr) => {{
        $crate::Profiler::counter_add($key, $delta);
    }};
}

/// Increment a named counter on the current span (aggregated by key), when a predicate is true.
///
/// Equivalent to `if pred { counter_add!(key, delta) }`, but reads more naturally at callsites
/// gated on a runtime flag.
///
/// ## Examples
///
/// ```rust
/// use stacks_profiler::{measure, counter_add_if};
///
/// let capture = true;
/// measure!("execute", {
///     counter_add_if!(capture, "runtime_cost", 500u64);
/// });
/// ```
#[macro_export]
macro_rules! counter_add_if {
    ($pred:expr, $key:literal, $delta:expr) => {{
        if $pred {
            $crate::Profiler::counter_add($key, $delta);
        }
    }};
}
