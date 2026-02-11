//! **basics** – A quick tour of every instrumentation macro.
//!
//! Run with:
//! ```sh
//! cargo run -p stacks-profiler --example basics
//! ```
//!
//! This example covers:
//! - `span!("name")` and `span!("name", tag)` – RAII guards
//! - `measure!("name", { .. })` and `measure!("name", tag, { .. })` – block-scoped spans
//! - `#[profile]` and `#[profile(name = "...")]` – attribute macros on functions
//! - `Profiler::take_results()` + `print_tree()` – extracting and displaying results

use stacks_profiler::{Profiler, profile};

// ── #[profile] attribute ─────────────────────────────────────────────────────

/// Uses the function name ("setup") as the span name.
#[profile]
fn setup() {
    std::hint::spin_loop(); // trivial work
}

/// Uses a custom span name.
#[profile(name = "Teardown Phase")]
fn teardown() {
    std::hint::spin_loop();
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    // 1. span!() – RAII guard, ended when `_guard` is dropped
    {
        let _guard = stacks_profiler::span!("Root Span");

        // 2. span!() with a tag – the tag appears in the tree as context
        {
            let _inner = stacks_profiler::span!("Tagged Span", "iteration-0");
            std::hint::spin_loop();
        }

        // 3. measure!() – wraps a block in a span
        stacks_profiler::measure!("Measured Block", {
            std::hint::spin_loop();
        });

        // 4. measure!() with a tag
        stacks_profiler::measure!("Measured With Tag", 42, {
            std::hint::spin_loop();
        });

        // 5. #[profile] attribute macros
        setup();
        teardown();
    }

    // Extract and print
    let results = Profiler::take_results();
    println!("\n=== basics ===\n");
    for root in &results {
        root.print_tree();
    }
}
