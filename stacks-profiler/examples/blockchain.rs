use std::thread;
use std::time::Duration;

use stacks_profiler::util::Flatten;
use stacks_profiler::{ProfileStats, Profiler};

// ============================================================================
// 1. Simulation Types
// ============================================================================

#[allow(unused)]
struct Transaction {
    id: usize,
    is_complex_contract: bool, // Simulates CPU heavy
    needs_disk_fetch: bool,    // Simulates I/O heavy (Wait)
}

struct Block {
    height: u64,
    txs: Vec<Transaction>,
}

// ============================================================================
// 3. Processing Logic
// ============================================================================

fn process_transaction(tx: &Transaction) {
    stacks_profiler::measure!("Execute Logic", {
        if tx.is_complex_contract {
            let _guard = stacks_profiler::span!("Clarity VM (CPU)");
            // Simulate CPU work (hashing, vm)
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(20) {}
        }

        if tx.needs_disk_fetch {
            let _guard = stacks_profiler::span!("State Fetch (Wait)");
            // Simulate Disk I/O
            thread::sleep(Duration::from_millis(50));
        }
    });
}

fn process_block(block: &Block) {
    for tx in &block.txs {
        stacks_profiler::measure!("Transaction", tx.id, {
            process_transaction(tx);
        });
    }
}

// ============================================================================
// 4. Main Scenario
// ============================================================================

fn main() {
    // Setup dummy data
    let blocks = vec![
        // Block 1: Fast, simple transactions
        Block {
            height: 1,
            txs: vec![
                Transaction {
                    id: 1,
                    is_complex_contract: false,
                    needs_disk_fetch: false,
                },
                Transaction {
                    id: 2,
                    is_complex_contract: false,
                    needs_disk_fetch: false,
                },
            ],
        },
        // Block 2: Contains a "Poison" transaction (Slow I/O)
        Block {
            height: 2,
            txs: vec![
                Transaction {
                    id: 1,
                    is_complex_contract: false,
                    needs_disk_fetch: false,
                },
                Transaction {
                    id: 2,
                    is_complex_contract: false,
                    needs_disk_fetch: true,
                }, // <--- SLOW
                Transaction {
                    id: 3,
                    is_complex_contract: true,
                    needs_disk_fetch: false,
                },
            ],
        },
    ];

    println!("Starting Blockchain Processing...");

    // Root scope
    {
        let _guard = stacks_profiler::span!("Chain Processing");

        for block in &blocks {
            stacks_profiler::measure!("Block", block.height, {
                process_block(block);
            });
        }
    }

    // Print results
    let results = Profiler::take_results();
    println!("\n=== Raw Profiler Trace ===\n");
    for r in &results {
        r.print_tree();
    }

    println!("\n=== Global Flat Profile ===\n");
    // "How much time did we spend in 'Execute Logic' across the ENTIRE program?"
    let global_flat = results.flatten();
    print_stats("Execute Logic", &global_flat);

    println!("\n=== Scoped Flat Profile ===\n");
    // Correct approach: Find 'Block 2' inside the GLOBAL flat list first.
    // The global flat list contains 'Block 2' with all its children aggregated inside it.
    if let Some(block_2) = global_flat.iter().find(|s| s.name() == "Block 2") {
        // Now we flatten just Block 2 to see the breakdown of ITS execution
        let block_2_flat = block_2.flatten();
        print_stats("Execute Logic", &block_2_flat);
    } else {
        println!("Could not find 'Block 2' in the trace.");
    }
}

fn print_stats(target_name: &str, flat_list: &[ProfileStats]) {
    if let Some(stat) = flat_list.iter().find(|s| s.id.name == target_name) {
        println!("Stats for '{}':", target_name);
        println!("  Total Count: {}", stat.count);
        println!("  Total Wall:  {:?}ns", stat.wall_time_ns);
        // The children of this flat node represent the aggregated breakdown
        // of what 'Execute Logic' called, across all its invocations.
    }
}
