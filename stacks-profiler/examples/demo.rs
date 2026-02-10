use std::thread;
use std::time::{Duration, Instant};

use stacks_profiler::{Profiler, profile};

// ── Helper Functions ─────────────────────────────────────────────────────────

/// Simulates pure CPU load (active work).
/// We use a busy loop instead of sleep to force the CPU time counter to tick.
fn burn_cpu(ms: u64) {
    let start = Instant::now();
    let target = Duration::from_millis(ms);
    while start.elapsed() < target {
        std::hint::spin_loop();
    }
}

/// Simulates I/O load (waiting).
/// The thread is suspended, so CPU time should be near 0, but Wall time increases.
fn simulate_io(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

// ── Profiled Functions ───────────────────────────────────────────────────────

#[profile(name = "Fetch Data (I/O Bound)")]
fn fetch_data_from_network() {
    // Simulate network latency
    simulate_io(100);
}

// Example of using profile_scope! with a named block
fn process_items(count: usize) {
    stacks_profiler::measure!("Process Batch (CPU Bound)", {
        for i in 0..count {
            // Create a nested scope for every generic iteration
            // (In a real app, you might not profile every single tight loop iteration
            // due to overhead, but this demonstrates the nesting capability)
            let _span = stacks_profiler::span!("Item Processing", i);
            burn_cpu(10); // 10ms of heavy math per item

            if i % 2 == 0 {
                // Every other item needs a quick lookup (I/O)
                let _span = stacks_profiler::span!("DB Lookup");
                simulate_io(5);
            }
        }
    });
}

#[profile] // Uses function name "save_results"
fn save_results() {
    // Simulate a mix of serialization (CPU) and disk write (Wait)
    {
        let _guard = stacks_profiler::span!("Serialize (CPU)");
        burn_cpu(20);
    }

    {
        let _guard = stacks_profiler::span!("Disk Write (Wait)");
        simulate_io(50);
    }
}

fn run_pipeline() {
    // We can wrap the entire workflow in a top-level scope
    let _guard = stacks_profiler::span!("Whole Pipeline");

    println!("  -> Fetching...");
    fetch_data_from_network();

    println!("  -> Processing...");
    process_items(3); // Process 3 items

    println!("  -> Saving...");
    save_results();
}

// ── Main Entrypoint ──────────────────────────────────────────────────────────

fn main() {
    println!("\n================ PROFILER DEMO ===================");

    // 1. Run the actual code
    run_pipeline();

    // 2. Extract results
    let results = Profiler::take_results();

    println!("\n================ PROFILER RESULTS ================");
    println!("Note: 'Wait' is highlighted in RED if it exceeds CPU time.");
    println!("      This indicates where your program is blocked.\n");

    // 3. Print the tree
    for root_node in results {
        root_node.print_tree();
    }
    println!("====================================================\n");
}
