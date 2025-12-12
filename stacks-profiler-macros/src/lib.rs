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
//! When a call is **not sampled**, the behavior is controlled by `sample_mode`:
//! - `sample_mode = "none"` (default): no guard is created (fastest, but may distort hierarchy)
//! - `sample_mode = "suppress"`: enters hierarchical suppression for this function call
//! - `sample_mode = "count_only"`: preserves hierarchy and increments count without timing
//!
//! **Note:** This crate is not intended to be used directly. You should use the re-exported
//! macro from the main `stacks-profiler` crate.

use darling::FromMeta;
use darling::ast::NestedMeta;
use proc_macro::TokenStream;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{ItemFn, Meta, parse_macro_input};

#[derive(Debug, Default, Clone, Copy, FromMeta)]
enum SampleMode {
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
    /// One of: "none" | "suppress" | "count_only".
    #[darling(default)]
    sample_mode: SampleMode,
}

/// Instruments a function by automatically creating a `stacks_profiler` span for its body.
///
/// This is the attribute-macro equivalent of placing a [`stacks_profiler::span!`] guard at the
/// top of a function and letting it drop on return (or panic).
///
/// The span name defaults to the function name and is scoped with a derived "context"
/// (roughly: the enclosing module/type path).
///
/// ## Usage
///
/// ```rust
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
/// ```rust
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
/// ```rust
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
/// ### Unsampled behavior (`sample_mode`)
///
/// When `sample_rate` is set and a given call is **not sampled**, the behavior is controlled
/// by `sample_mode`:
///
/// - `sample_mode = "none"` (default): returns `None` (no guard). This is the cheapest path,
///   but nested `span!` calls may attach to the nearest sampled ancestor if the parent function
///   call is unsampled.
/// - `sample_mode = "suppress"`: enters *hierarchical suppression* for the duration of the
///   unsampled call. Nested spans become no-ops, preventing wrong-parent attachment, but also
///   dropping nested detail under unsampled parents.
/// - `sample_mode = "count_only"`: preserves hierarchy by pushing a lightweight frame and
///   increments `count` without reading clocks. This keeps nested spans correctly parented and
///   yields accurate per-context call counts, at higher overhead than `"suppress"`/`"none"`.
///
/// Examples:
///
/// ```rust
/// use stacks_profiler::profile;
///
/// #[profile(sample_rate = 100, sample_mode = "suppress")]
/// fn request() {
///     // nested spans won't attach to the wrong parent on unsampled calls
/// }
///
/// #[profile(sample_rate = 100, sample_mode = "count_only")]
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

    let vis = &input_fn.vis;
    let sig = &input_fn.sig;
    let block = &input_fn.block;

    let context_extraction = quote! {
        let type_name = std::any::type_name::<__StacksProfilerScope>();
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
    };

    let setup_block = match args.name {
        Some(custom_name) => {
            quote! {
                {
                    struct __StacksProfilerScope;

                    static __PROFILER_SPAN_ID: std::sync::OnceLock<stacks_profiler::SpanId> =
                        std::sync::OnceLock::new();
                    __PROFILER_SPAN_ID.get_or_init(|| {
                        #context_extraction
                        stacks_profiler::Profiler::new_span_id(#custom_name).with_context(context)
                    })
                }
            }
        }
        None => {
            quote! {
                {
                    struct __StacksProfilerScope;

                    static __PROFILER_SPAN_ID: std::sync::OnceLock<stacks_profiler::SpanId> =
                        std::sync::OnceLock::new();
                    __PROFILER_SPAN_ID.get_or_init(|| {
                        #context_extraction
                        stacks_profiler::Profiler::new_span_id(auto_name).with_context(context)
                    })
                }
            }
        }
    };

    let mode = args.sample_mode;
    let guard_creation = if let Some(rate) = args.sample_rate {
        if rate <= 1 {
            quote! {
                let __profiler_guard =
                    if stacks_profiler::Profiler::is_suppressed() {
                        None
                    } else {
                        Some(stacks_profiler::Profiler::begin_span(__profiler_span_id, None))
                    };
            }
        } else if rate.is_power_of_two() {
            let mask = rate - 1;

            // Unsampled behavior selection
            let unsampled = match mode {
                SampleMode::None => quote! { None },
                SampleMode::Suppress => {
                    quote! { Some(stacks_profiler::Profiler::begin_suppression()) }
                }
                SampleMode::CountOnly => {
                    quote! { Some(stacks_profiler::Profiler::begin_span_count_only(__profiler_span_id, None)) }
                }
            };

            quote! {
                static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);

                let __n = __PROFILER_SAMPLE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let __should_sample = (__n & #mask) == 0;

                let __profiler_guard =
                    if stacks_profiler::Profiler::is_suppressed() {
                        None
                    } else if __should_sample {
                        Some(stacks_profiler::Profiler::begin_span(__profiler_span_id, None))
                    } else {
                        #unsampled
                    };
            }
        } else {
            let unsampled = match mode {
                SampleMode::None => quote! { None },
                SampleMode::Suppress => {
                    quote! { Some(stacks_profiler::Profiler::begin_suppression()) }
                }
                SampleMode::CountOnly => {
                    quote! { Some(stacks_profiler::Profiler::begin_span_count_only(__profiler_span_id, None)) }
                }
            };

            quote! {
                static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);

                let __n = __PROFILER_SAMPLE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let __should_sample = (__n % #rate) == 0;

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
    } else {
        // Always timed (unless suppressed).
        quote! {
            let __profiler_guard =
                if stacks_profiler::Profiler::is_suppressed() {
                    None
                } else {
                    Some(stacks_profiler::Profiler::begin_span(__profiler_span_id, None))
                };
        }
    };

    let output = quote! {
        #vis #sig {
            let __profiler_span_id = #setup_block;
            #guard_creation
            #block
        }
    };

    output.into()
}
