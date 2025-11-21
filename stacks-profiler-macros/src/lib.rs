//! # Stacks Profiler Macros
//!
//! This crate provides the procedural macros for the `stacks-profiler` crate.
//!
//! It exports the `#[profile]` attribute, which automatically instruments functions
//! to measure Wall Time, CPU Time, and Wait Time.
//!
//! **Note:** This crate is not intended to be used directly. You should use the re-exported
//! macro from the main `stacks-profiler` crate.

use proc_macro::TokenStream;
use darling::{FromMeta, ast::NestedMeta}; // Import NestedMeta
use quote::quote;
use syn::{parse_macro_input, ItemFn, Meta};
use syn::punctuated::Punctuated;
use syn::token::Comma;

#[derive(Debug, FromMeta)]
struct ProfileArgs {
    #[darling(default)]
    name: Option<String>,
}

/// Instruments a function to be tracked by the global `Profiler`.
///
/// This macro wraps the function body in a new block that:
/// 1. Starts a profiling span immediately upon entry.
/// 2. Creates a RAII guard to ensure the span is closed when the function returns (or panics).
///
/// # Arguments
///
/// * `name` (optional): Overrides the span name. If omitted, the function name is used.
///
/// # Examples
///
/// ## Basic Usage
/// Uses the function name ("process_data") as the span name.
/// ```ignore
/// #[profile]
/// fn process_data() {
///     // ... work ...
/// }
/// ```
///
/// ## Custom Name
/// Useful for grouping overloaded functions or providing more context.
/// ```ignore
/// #[profile(name = "Data Processing - Phase 1")]
/// fn process_data() {
///     // ... work ...
/// }
/// ```
///
/// # Async Safety Warning
/// The underlying profiler uses **Thread Local Storage**.
/// * **Safe:** Synchronous functions.
/// * **Safe:** `async` functions running on a single-threaded runtime.
/// * **Unsafe:** `async` functions running on a multi-threaded work-stealing runtime (like Tokio's default).
///   If the task moves between threads, the start/end measurements will happen on different
///   thread stacks, leading to panic or corrupted data.
#[proc_macro_attribute]
pub fn profile(args: TokenStream, input: TokenStream) -> TokenStream {
    // 1. Parse the attributes as a list of Meta items (e.g. name="foo", key=value)
    let attr_args = parse_macro_input!(args with Punctuated::<Meta, Comma>::parse_terminated);
    
    // 2. Convert syn::Meta to darling::ast::NestedMeta
    // Darling expects NestedMeta (which can be a Meta or a Literal), so we wrap our Metas.
    let args_vec: Vec<NestedMeta> = attr_args
        .into_iter()
        .map(NestedMeta::Meta)
        .collect();

    // 3. Process with Darling
    let args = match ProfileArgs::from_list(&args_vec) {
        Ok(v) => v,
        Err(e) => {
            return TokenStream::from(e.write_errors());
        }
    };

    // 4. Parse the function body
    let input_fn = parse_macro_input!(input as ItemFn);

    let vis = &input_fn.vis;
    let sig = &input_fn.sig;
    let block = &input_fn.block;
    
    // 5. Determine the span name
    let span_name = match args.name {
        Some(n) => n,
        None => sig.ident.to_string(),
    };

    // 6. Generate the new function body
    let output = quote! {
        #vis #sig {
            let _guard = stacks_profiler::Profiler::begin_span(#span_name);
            #block
        }
    };

    output.into()
}