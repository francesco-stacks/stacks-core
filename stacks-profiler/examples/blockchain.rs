use std::thread;
use std::time::Duration;

use stacks_profiler::Profiler;

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
}
