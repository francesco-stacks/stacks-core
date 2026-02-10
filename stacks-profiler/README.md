# Stacks Profiler

A lightweight, thread-local profiler for Rust that measures **wall time**,
**CPU time**, and **wait time** (wall − CPU) with minimal overhead.

Designed for performance-critical code where understanding the distinction
between "working" (burning CPU) and "waiting" (disk I/O, network, mutex
contention) is vital.

## Features

* **Dual Timing** — captures wall-clock and per-thread CPU time simultaneously.
* **Wait Time** — automatically derives `wait = wall − CPU`.
* **Nested Spans** — supports deep call stacks with hierarchical reporting.
* **Tags** — distinguish spans at the same callsite (e.g., transaction index).
* **Records & Counters** — attach key/value data and additive metrics to spans.
* **Sampling** — `rate: N` skips N−1 out of every N entries at a callsite.
* **Macros** — `#[profile]`, `span!`, `measure!`, `record!`, `counter_add!`.
* **Platform Support**:
  * **macOS** — `clock_gettime_nsec_np` (sub-µs resolution).
  * **Linux** — `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` (sub-µs resolution).
  * **Windows** — `GetThreadTimes` (~15.6 ms resolution; see crate docs).

## Usage

### 1. Function-Level Profiling

```rust
use stacks_profiler::{profile, Profiler};

#[profile]
fn heavy_processing() {
    // Your code here...
}

#[profile(name = "Custom I/O Label")]
fn load_data() {
    std::thread::sleep(std::time::Duration::from_millis(100));
}
```

### 2. Block-Level Profiling

Use `span!` or `measure!` to profile specific regions.

```rust
use stacks_profiler::{span, measure};

fn complex_logic() {
    // span! returns an Option<ProfileGuard> — the span lives until the
    // guard is dropped.
    let _guard = span!("Inner Loop");
    for _ in 0..1000 {
        // ... work ...
    }
}

fn with_measure() {
    // measure! wraps a block and returns its value.
    let result = measure!("Computation", {
        42
    });
}
```

### 3. Tags, Records, and Counters

```rust
use stacks_profiler::{span, measure, record, counter_add};

fn process_block(height: u64) {
    // Tag: distinguishes this span from others at the same callsite.
    measure!("Block", height, {
        record!("block_height", height);
        counter_add!("bytes_processed", 4096);
        // ... work ...
    });
}
```

### 4. Analyzing Results

Results are stored in thread-local storage.  Retrieve them explicitly:

```rust
use stacks_profiler::Profiler;

fn main() {
    // ... run profiled code ...

    let results = Profiler::take_results();
    for root in &results {
        root.print_tree();
    }
}
```

### Output Example

`print_tree` produces a colourised, hierarchical view:

```text
Chain Processing [total: 75.012ms | busy: 20.035ms | wait: 54.977ms] (x1) @ stacks-profiler/examples/blockchain.rs:100
├── ▶ Block #1 [total: 0.002ms | busy: 0.002ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:103
│   ├── ▶ Transaction #1 [total: 0.001ms | busy: 0.001ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:44
│   │   └── ▶ Execute Logic [total: 0.000ms | busy: 0.000ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:26
│   └── ▶ Transaction #2 [total: 0.001ms | busy: 0.001ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:44
│       └── ▶ Execute Logic [total: 0.000ms | busy: 0.000ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:26
└── ▶ Block #2 [total: 74.996ms | busy: 20.019ms | wait: 54.977ms] (x1) @ stacks-profiler/examples/blockchain.rs:103
    ├── ▶ Transaction #1 [total: 0.001ms | busy: 0.001ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:44
    │   └── ▶ Execute Logic [total: 0.000ms | busy: 0.000ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:26
    ├── ▶ Transaction #2 [total: 54.986ms | busy: 0.009ms | wait: 54.977ms] (x1) @ stacks-profiler/examples/blockchain.rs:44
    │   └── ▶ Execute Logic [total: 54.986ms | busy: 0.009ms | wait: 54.977ms] (x1) @ stacks-profiler/examples/blockchain.rs:26
    │       └── ▶ State Fetch (Wait) [total: 54.985ms | busy: 0.008ms | wait: 54.977ms] (x1) @ stacks-profiler/examples/blockchain.rs:35
    └── ▶ Transaction #3 [total: 20.009ms | busy: 20.009ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:44
        └── ▶ Execute Logic [total: 20.001ms | busy: 20.001ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:26
            └── ▶ Clarity VM (CPU) [total: 20.000ms | busy: 20.000ms | wait: 0.000ms] (x1) @ stacks-profiler/examples/blockchain.rs:28
```

## Examples

Run the included examples:

```sh
cargo run -p stacks-profiler --example demo
cargo run -p stacks-profiler --example loops
cargo run -p stacks-profiler --example blockchain
```
