//! # Stacks Profiler Macros
//!
//! This crate provides the procedural macros for the `stacks-profiler` crate.
//!
//! It exports the `#[profile]` attribute, which automatically instruments functions
//! to measure Wall Time, CPU Time, and Wait Time.
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

#[derive(Debug, FromMeta)]
struct ProfileArgs {
    #[darling(default)]
    name: Option<String>,
}

/// Instruments a function to be tracked by the global `Profiler`.
#[proc_macro_attribute]
pub fn profile(args: TokenStream, input: TokenStream) -> TokenStream {
    // Parse the attributes as a list of Meta items (e.g. name="foo", key=value)
    let attr_args = parse_macro_input!(args with Punctuated::<Meta, Comma>::parse_terminated);

    // Convert syn::Meta to darling::ast::NestedMeta
    let args_vec: Vec<NestedMeta> = attr_args.into_iter().map(NestedMeta::Meta).collect();

    // Process with Darling
    let args = match ProfileArgs::from_list(&args_vec) {
        Ok(v) => v,
        Err(e) => {
            return TokenStream::from(e.write_errors());
        }
    };

    // Parse the function body
    let input_fn = parse_macro_input!(input as ItemFn);

    let vis = &input_fn.vis;
    let sig = &input_fn.sig;
    let block = &input_fn.block;

    // Logic to extract and clean context (runs once per function via OnceLock)
    // We use the "type_name hack" to extract the full path.
    let context_extraction = quote! {
        let type_name = std::any::type_name::<__StacksProfilerScope>();
        // type_name: "path::to::Type::func::__StacksProfilerScope"

        // Strip suffix "::__StacksProfilerScope" (23 chars)
        let full_path = &type_name[..type_name.len() - 23];

        // Split into context (module/type) and name (function)
        let (mut context, auto_name) = match full_path.rfind("::") {
            Some(idx) => (&full_path[..idx], &full_path[idx+2..]),
            None => ("", full_path),
        };

        // Handle Trait Impls: <Type as Trait> -> Type
        if context.starts_with('<') {
            if let Some(idx) = context.find(" as ") {
                context = &context[1..idx];
            }
        }

        // Handle Generics: Type<T> -> Type
        let last_colon = context.rfind("::").map(|i| i + 2).unwrap_or(0);
        if let Some(idx) = context[last_colon..].find('<') {
            context = &context[..last_colon + idx];
        }
    };

    // Generate the span setup statement.
    // FIX: We define `struct __StacksProfilerScope` *before* the `get_or_init` call.
    let setup_block = match args.name {
        Some(custom_name) => {
            quote! {
                {
                    // Defined here so it belongs to the function scope, not the closure scope
                    struct __StacksProfilerScope;

                    static __PROFILER_SPAN_ID: std::sync::OnceLock<stacks_profiler::SpanId> = std::sync::OnceLock::new();
                    __PROFILER_SPAN_ID.get_or_init(|| {
                        #context_extraction
                        stacks_profiler::Profiler::new_span_id(#custom_name)
                            .with_context(context)
                    })
                }
            }
        }
        None => {
            quote! {
                {
                    // Defined here so it belongs to the function scope, not the closure scope
                    struct __StacksProfilerScope;

                    static __PROFILER_SPAN_ID: std::sync::OnceLock<stacks_profiler::SpanId> = std::sync::OnceLock::new();
                    __PROFILER_SPAN_ID.get_or_init(|| {
                        #context_extraction
                        stacks_profiler::Profiler::new_span_id(auto_name)
                            .with_context(context)
                    })
                }
            }
        }
    };

    let output = quote! {
        #vis #sig {
            let __profiler_span_id = #setup_block;
            let __profiler_guard = stacks_profiler::Profiler::begin_span(__profiler_span_id, None);
            #block
        }
    };

    output.into()
}
