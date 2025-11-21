# Stacks Profiler

A lightweight, thread-local profiler for Rust designed to measure **Wall Time**, **CPU Time**, and **I/O Wait Time** (Blocking Time) on macOS and Linux.

It is designed for performance-critical code where understanding the distinction between "working" (burning CPU) and "waiting" (disk I/O, network, mutex contention) is vital.

## Features

* **Dual Timing**: Captures both Wall clock and CPU clock simultaneously.
* **Wait Time Calculation**: Automatically derives `Wait = Wall - CPU`.
* **Nested Spans**: Supports deep call stacks and hierarchical reporting.
* **Macros**: Easy-to-use attribute `#[profile]` and block-level `profile_scope!`.
* **Platform Support**:
  * **Linux**: Uses `CLOCK_THREAD_CPUTIME_ID`.
  * **macOS**: Uses `CLOCK_THREAD_CPUTIME_ID` (available on macOS Sierra 10.12+).

## Usage

### 1. Function-Level Profiling

Use the attribute macro to automatically profile an entire function.

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

Use the macro to profile specific regions within a function.

```rust
use stacks_profiler::profile_scope;

fn complex_logic() {
    // ... setup ...
    
    {
        profile_scope!("Inner Loop");
        for _ in 0..1000 {
            // ... work ...
        }
    }
}
```

### 3. Analyzing Results

Results are stored in Thread Local Storage (TLS). You must retrieve them explicitly.

```rust
fn main() {
    // Run your code
    heavy_processing();
    load_data();

    // Extract and print metrics
    let results = Profiler::take_results();
    for root_span in results {
        root_span.print_tree(0);
    }
}
```

### Output Example

The `print_tree` method provides a colorized, hierarchical view:

```text
├─ [Main] Wall: 150.0ms | CPU: 50.0ms | Wait: 100.0ms
  ├─ [Computation] Wall: 49.0ms | CPU: 49.0ms | Wait: 0.0ms
  ├─ [Database IO] Wall: 100.5ms | CPU: 0.5ms | Wait: 100.0ms
```
