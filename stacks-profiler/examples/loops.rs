use stacks_profiler::Profiler;

fn sub_task() {
    let _span = stacks_profiler::span!("Sub Task");
    // Do a little work
    let mut _x = 0;
    for _ in 0..1000 {
        _x += 1;
    }
}

fn main() {
    println!("Running Loop Benchmark...");

    {
        let _span = stacks_profiler::span!("Main Loop");

        // This loop runs 100 times.
        // Without aggregation, the tree would have 100 entries.
        // With aggregation, we expect 1 entry with count=100.
        for i in 0..100 {
            let _span = stacks_profiler::span!("Iteration");

            sub_task();

            // Simulate some variation
            if i % 10 == 0 {
                // This call site is DIFFERENT (different line number),
                // so it should appear as a separate node in the tree.
                sub_task();
            }
        }
    }

    let results = Profiler::take_results();
    for r in results {
        r.print_tree();
    }
}
