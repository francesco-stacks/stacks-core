use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use stacks_profiler::{Profiler, measure, span};

fn bench_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("Profiler Overhead");

    // 1. Baseline: How fast is a raw function call?
    // We need this to subtract from the profiler results to find the pure overhead.
    group.bench_function("baseline_noop", |b| {
        b.iter(|| {
            black_box(());
        });
    });

    // 2. Untagged Span Overhead
    // This measures the cost of:
    // - Thread Local access
    // - Instant::now() x2 (start/end)
    // - CPU timer x2
    // - Stack push/pop
    // - Vector recycling logic
    group.bench_function("span_untagged", |b| {
        // We define the ID outside the loop to simulate the OnceLock behavior
        // of the macros, ensuring we only measure the runtime cost, not initialization.
        let id = Box::leak(Box::new(Profiler::new_span_id("bench")));

        // Warm up OnceLock / TLS by doing one span outside measurement
        {
            let _guard = Profiler::begin_span(id, None);
            black_box(());
        }

        b.iter(|| {
            let _guard = Profiler::begin_span(id, None);
            black_box(());
        });
        Profiler::clear();
    });

    // 3. Tagged Span Overhead
    // Adds the cost of constructing and storing the Tag enum.
    group.bench_function("span_tagged_u64", |b| {
        let id = Box::leak(Box::new(Profiler::new_span_id("bench_tag")));

        // Warm up OnceLock / TLS by doing one span outside measurement
        {
            let _guard = Profiler::begin_span(id, None);
            black_box(());
        }

        b.iter(|| {
            let _guard = Profiler::begin_span(id, Some(12345u64.into()));
            black_box(());
        });
        Profiler::clear();
    });

    // 4. Macro Overhead
    // Measures the full cost including the OnceLock check inside the macro.
    group.bench_function("macro_span", |b| {
        b.iter(|| {
            let _guard = span!("macro_bench");
            black_box(());
        });
        Profiler::clear();
    });

    // 5. Nested Overhead (Depth 3)
    // This tests pushing to stack, popping, and merging into PARENT (not root).
    group.bench_function("nested_depth_3", |b| {
        b.iter(|| {
            measure!("root", {
                measure!("child", {
                    measure!("grandchild", {
                        black_box(());
                    })
                })
            })
        });
        Profiler::clear();
    });

    // 6. Sibling Merge Overhead
    // This tests the "Hot Loop" scenario where we merge into the same sibling repeatedly.
    // This validates the `if last.id == stats.id` optimization in `merge_into_list`.
    group.bench_function("sibling_merge_loop", |b| {
        b.iter(|| {
            measure!("root", {
                for _ in 0..10 {
                    measure!("child", { black_box(()) });
                }
            })
        });
        Profiler::clear();
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = bench_overhead
}
criterion_main!(benches);
