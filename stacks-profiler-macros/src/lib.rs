//! # Stacks Profiler Macros
//!
//! This crate provides the procedural macros for the `stacks-profiler` crate.
//!
//! It exports the `#[profile]` attribute, which automatically instruments functions
//! to measure Wall Time, CPU Time, and Wait Time.
//!
//! ## Sampling behavior
//!
//! `#[profile(sample_rate = N)]` samples approximately 1 out of every N calls per callsite.
//!
//! When a call is **not sampled**, the behavior is controlled by `unsampled`:
//! - `unsampled = "none"` (default): no guard is created (fastest, but may distort hierarchy)
//! - `unsampled = "suppress"`: enters hierarchical suppression for this function call
//! - `unsampled = "count_only"`: preserves hierarchy and increments count without timing
//!
//! **Note:** This crate is not intended to be used directly. You should use the re-exported
//! macro from the main `stacks-profiler` crate.

use darling::FromMeta;
use darling::ast::NestedMeta;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{ItemFn, Meta, parse_macro_input};

// ── Attribute Definitions ────────────────────────────────────────────────────

/// Defines behavior for unsampled calls when `sample_rate` is set.
#[derive(Debug, Default, Clone, Copy, FromMeta)]
enum UnsampledBehavior {
    /// Unsampled calls return `None` (default).
    #[default]
    #[darling(rename = "none")]
    None,
    /// Unsampled calls enter hierarchical suppression (nested spans become no-ops).
    #[darling(rename = "suppress")]
    Suppress,
    /// Unsampled calls preserve hierarchy + increment counts without timing.
    #[darling(rename = "count_only")]
    CountOnly,
}

/// Arguments for the `#[profile]` attribute macro.
#[derive(Debug, FromMeta)]
struct ProfileArgs<Name, SampleRate>
where
    Name: Into<Option<String>> + Default,
    SampleRate: Into<Option<usize>> + Default,
{
    #[darling(default)]
    name: Name,

    #[darling(default)]
    sample_rate: SampleRate,

    /// Controls what happens on *unsampled* calls when `sample_rate` is set.
    ///
    /// One of: "none" | "suppress" | "count_only".
    #[darling(default)]
    unsampled: UnsampledBehavior,
}

// ── Codegen Helpers ──────────────────────────────────────────────────────────

/// Emit the runtime code that derives `context` and `auto_name` from
/// `std::any::type_name::<__StacksProfilerScope>()`.
///
/// The generated code expects a struct named `__StacksProfilerScope` to be in
/// scope (defined by [`build_setup_block`]).
fn build_context_extraction() -> TokenStream2 {
    quote! {
        let type_name = std::any::type_name::<__StacksProfilerScope>();
        // Strip the "::__StacksProfilerScope" suffix (23 chars) to get the enclosing path.
        let full_path = &type_name[..type_name.len() - 23];

        let (mut context, auto_name) = match full_path.rfind("::") {
            Some(idx) => (&full_path[..idx], &full_path[idx+2..]),
            None => ("", full_path),
        };

        if context.starts_with('<') {
            if let Some(idx) = context.find(" as ") {
                context = &context[1..idx];
            }
        }

        let last_colon = context.rfind("::").map(|i| i + 2).unwrap_or(0);
        if let Some(idx) = context[last_colon..].find('<') {
            context = &context[..last_colon + idx];
        }
    }
}

/// Emit the block that initialises a `static OnceLock<SpanId>` and returns
/// a reference to the span id.
///
/// If `name` is `Some`, the span uses that literal; otherwise it derives
/// the name from the enclosing function via `auto_name`.
fn build_setup_block(name: Option<String>) -> TokenStream2 {
    let context_extraction = build_context_extraction();

    let span_id_init = match name {
        Some(custom_name) => quote! {
            stacks_profiler::Profiler::new_span_id(#custom_name).with_context(context)
        },
        None => quote! {
            stacks_profiler::Profiler::new_span_id(auto_name).with_context(context)
        },
    };

    quote! {
        {
            struct __StacksProfilerScope;

            static __PROFILER_SPAN_ID: std::sync::OnceLock<stacks_profiler::SpanId> =
                std::sync::OnceLock::new();
            __PROFILER_SPAN_ID.get_or_init(|| {
                #context_extraction
                #span_id_init
            })
        }
    }
}

/// Emit the expression used when a sampled call is **not** selected.
///
/// This is the `else` branch of the sampling `if`; it varies by
/// [`UnsampledBehavior`].
fn unsampled_guard_expr(mode: UnsampledBehavior) -> TokenStream2 {
    match mode {
        UnsampledBehavior::None => quote! { None },
        UnsampledBehavior::Suppress => {
            quote! { Some(stacks_profiler::Profiler::begin_suppression()) }
        }
        UnsampledBehavior::CountOnly => {
            quote! { Some(stacks_profiler::Profiler::begin_span_count_only(__profiler_span_id, None)) }
        }
    }
}

/// Emit the code that creates `__profiler_guard` (an `Option<ProfileGuard>`).
///
/// The generated code assumes `__profiler_span_id` is already in scope
/// (produced by [`build_setup_block`]).
///
/// * `rate = None` → always timed (unless suppressed).
/// * `rate = Some(0 | 1)` → same as always timed.
/// * `rate = Some(n)` → sampled 1/n; uses a bitmask when `n` is a power of two.
fn build_guard_creation(sample_rate: Option<usize>, mode: UnsampledBehavior) -> TokenStream2 {
    let always_timed = quote! {
        let __profiler_guard =
            if stacks_profiler::Profiler::is_suppressed() {
                None
            } else {
                Some(stacks_profiler::Profiler::begin_span(__profiler_span_id, None))
            };
    };

    let Some(rate) = sample_rate else {
        return always_timed;
    };

    if rate <= 1 {
        return always_timed;
    }

    let unsampled = unsampled_guard_expr(mode);

    // The sampling check: bitmask for power-of-two rates, modulo otherwise.
    let should_sample = if rate.is_power_of_two() {
        let mask = rate - 1;
        quote! { (__n & #mask) == 0 }
    } else {
        quote! { (__n % #rate) == 0 }
    };

    quote! {
        static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);

        let __n = __PROFILER_SAMPLE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let __should_sample = #should_sample;

        let __profiler_guard =
            if stacks_profiler::Profiler::is_suppressed() {
                None
            } else if __should_sample {
                Some(stacks_profiler::Profiler::begin_span(__profiler_span_id, None))
            } else {
                #unsampled
            };
    }
}

// ── Main Entrypoint ──────────────────────────────────────────────────────────

/// Instruments a function by automatically creating a `stacks_profiler` span for its body.
///
/// This is the attribute-macro equivalent of placing a `stacks_profiler::span!` guard at the
/// top of a function and letting it drop on return (or panic).
///
/// The span name defaults to the function name and is scoped with a derived "context"
/// (roughly: the enclosing module/type path).
///
/// ## Usage
///
/// ```rust,ignore
/// use stacks_profiler::profile;
///
/// #[profile]
/// fn parse_block() {
///     // ...timed...
/// }
/// ```
///
/// You can override the span name:
///
/// ```rust,ignore
/// use stacks_profiler::profile;
///
/// #[profile(name = "net.rx")]
/// fn recv_packet() {
///     // ...timed...
/// }
/// ```
///
/// ## Sampling (`sample_rate`)
///
/// You can sample a hot function so only ~1 out of every `N` calls is timed:
///
/// ```rust,ignore
/// use stacks_profiler::profile;
///
/// // Roughly 1% of calls are timed at this callsite.
/// #[profile(sample_rate = 100)]
/// fn hot_path() {
///     // ...timed sometimes...
/// }
/// ```
///
/// Sampling is **per-callsite** and uses a `static AtomicUsize` counter (Relaxed ordering).
///
/// ### Unsampled behavior (`unsampled`)
///
/// When `sample_rate` is set and a given call is **not sampled**, the behavior is controlled
/// by `unsampled`:
///
/// - `unsampled = "none"` (default): returns `None` (no guard). This is the cheapest path,
///   but nested `span!` calls may attach to the nearest sampled ancestor if the parent function
///   call is unsampled.
/// - `unsampled = "suppress"`: enters *hierarchical suppression* for the duration of the
///   unsampled call. Nested spans become no-ops, preventing wrong-parent attachment, but also
///   dropping nested detail under unsampled parents.
/// - `unsampled = "count_only"`: preserves hierarchy by pushing a lightweight frame and
///   increments `count` without reading clocks. This keeps nested spans correctly parented and
///   yields accurate per-context call counts, at higher overhead than `"suppress"`/`"none"`.
///
/// Examples:
///
/// ```rust,ignore
/// use stacks_profiler::profile;
///
/// #[profile(sample_rate = 100, unsampled = "suppress")]
/// fn request() {
///     // nested spans won't attach to the wrong parent on unsampled calls
/// }
///
/// #[profile(sample_rate = 100, unsampled = "count_only")]
/// fn execute_tx() {
///     // preserves hierarchy + counts even when this call is not timed
/// }
/// ```
///
/// ## Notes / limitations
///
/// - This macro currently does **not** attach a tag (it instruments with `tag = None`).
///   Use `span!` directly if you need tags at the callsite.
/// - If suppression is active (entered by an ancestor span using suppression), this macro
///   emits no guard for the current function call.
/// - The span guard is a local variable; it is dropped when the function returns (including
///   early returns) or when unwinding due to panic.
#[proc_macro_attribute]
pub fn profile(args: TokenStream, input: TokenStream) -> TokenStream {
    let attr_args = parse_macro_input!(args with Punctuated::<Meta, Comma>::parse_terminated);
    let args_vec: Vec<NestedMeta> = attr_args.into_iter().map(NestedMeta::Meta).collect();

    let args = match ProfileArgs::from_list(&args_vec) {
        Ok(v) => v,
        Err(e) => return TokenStream::from(e.write_errors()),
    };

    let input_fn = parse_macro_input!(input as ItemFn);

    let attrs = &input_fn.attrs;
    let vis = &input_fn.vis;
    let sig = &input_fn.sig;
    let block = &input_fn.block;

    let setup_block = build_setup_block(args.name);
    let guard_creation = build_guard_creation(args.sample_rate, args.unsampled);

    let output = quote! {
        #(#attrs)*
        #vis #sig {
            let __profiler_span_id = #setup_block;
            #guard_creation
            #block
        }
    };

    output.into()
}
