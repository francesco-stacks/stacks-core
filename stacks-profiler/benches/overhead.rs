use std::hint::black_box;

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use stacks_profiler::{Profiler, measure, span, record};

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

    // Tagged Span Overhead
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

    // Macro Overhead
    // Measures the full cost including the OnceLock check inside the macro.
    group.bench_function("macro_span", |b| {
        b.iter(|| {
            let _guard = span!("macro_bench");
            black_box(());
        });
        Profiler::clear();
    });

    // Nested Overhead (Depth 3)
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

    // Sibling Merge Overhead
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

    // Sampled Span (10% sampling)
    // Should be significantly faster than untagged
    group.bench_function("span_sampled_10", |b| {
        b.iter(|| {
            let _guard = span!("sampled_10", rate: 10);
            black_box(());
        });
        Profiler::clear();
    });

    // Sampled Span (1% sampling)
    // Should be nearly as fast as baseline
    group.bench_function("span_sampled_100", |b| {
        b.iter(|| {
            let _guard = span!("sampled_100", rate: 100);
            black_box(());
        });
        Profiler::clear();
    });

    // Suppressed unsampled parent:
    // - If not sampled, we enter suppression and *nested spans become no-ops*.
    group.bench_function("span_sampled_100_suppress_parent", |b| {
        b.iter(|| {
            let _guard = span!("sampled_100_suppress_parent", rate: 100, suppress);
            black_box(());
        });
        Profiler::clear();
    });

    // Count-only unsampled parent:
    // - If not sampled, we still push a lightweight frame to preserve hierarchy,
    //   increment per-context count, but do not read clocks.
    group.bench_function("span_sampled_100_count_only_parent", |b| {
        b.iter(|| {
            let _guard = span!("sampled_100_count_only_parent", rate: 100, count_only);
            black_box(());
        });
        Profiler::clear();
    });

    // Demonstrates the hierarchy issue explicitly:
    // Suppression means children don't attach to the wrong parent (they are dropped).
    group.bench_function("nested_parent_unsampled_suppress_children", |b| {
        b.iter(|| {
            measure!("root", {
                let _p = span!("parent", rate: 100, suppress);
                // Child work that would otherwise attach to root if parent is unsampled.
                let _c = span!("child");
                black_box(());
            });
        });
        Profiler::clear();
    });

    // Count-only means children still attach under the parent, preserving tree context.
    group.bench_function("nested_parent_unsampled_count_only_children", |b| {
        b.iter(|| {
            measure!("root", {
                let _p = span!("parent", rate: 100, count_only);
                let _c = span!("child");
                black_box(());
            });
        });
        Profiler::clear();
    });

    // Optional: tagged variants (root fanout is often tag-driven)
    group.bench_function("span_sampled_100_suppress_tagged_u64", |b| {
        b.iter(|| {
            let _guard = span!("sampled_100_suppress_tagged_u64", 12345u64, rate: 100, suppress);
            black_box(());
        });
        Profiler::clear();
    });

    group.bench_function("span_sampled_100_count_only_tagged_u64", |b| {
        b.iter(|| {
            let _guard =
                span!("sampled_100_count_only_tagged_u64", 12345u64, rate: 100, count_only);
            black_box(());
        });
        Profiler::clear();
    });

    group.finish();
}

fn bench_10m_sampled_spans(c: &mut Criterion) {
    let mut group = c.benchmark_group("Profiler 10M sampled spans");

    // Use flat sampling: explicitly tell Criterion to run exactly 10 samples. This overrides the
    // auto-tuning logic that tries to fit 100 samples into a time window. This works well here
    // since we're manually running a large number of iterations internally for each sample.
    group.sample_size(10).sampling_mode(SamplingMode::Flat);

    group.bench_function("10M_calls_sampled_10", |b| {
        b.iter(|| {
            let _outer_guard = span!("outer_loop");
            for _ in 0..10_000_000u32 {
                let _guard = span!("sampled_10", rate: 10);
                black_box(());
            }
            Profiler::clear();
        });
    });

    group.bench_function("10M_calls_sampled_100", |b| {
        b.iter(|| {
            let _outer_guard = span!("outer_loop");
            for _ in 0..10_000_000u32 {
                let _guard = span!("sampled_100", rate: 100);
                black_box(());
            }
            Profiler::clear();
        });
    });

    group.bench_function("10M_calls_sampled_100_suppress_parent", |b| {
        b.iter(|| {
            let _outer_guard = span!("outer_loop");
            for _ in 0..10_000_000u32 {
                let _guard = span!("sampled_100_suppress_parent", rate: 100, suppress);
                black_box(());
            }
            Profiler::clear();
        });
    });

    group.bench_function("10M_calls_sampled_100_count_only_parent", |b| {
        b.iter(|| {
            let _outer_guard = span!("outer_loop");
            for _ in 0..10_000_000u32 {
                let _guard = span!("sampled_100_count_only_parent", rate: 100, count_only);
                black_box(());
            }
            Profiler::clear();
        });
    });

    group.finish();
}

fn bench_record(c: &mut Criterion) {
    let mut group = c.benchmark_group("Profiler Record Overhead");

    // Record overhead: no span (should be near-zero due to early return)
    group.bench_function("record_no_span", |b| {
        b.iter(|| {
            record!("k", 123u64);
            black_box(());
        });
    });

    // Record overhead: span + record u64 (no allocation)
    group.bench_function("record_u64", |b| {
        b.iter(|| {
            let _g = span!("record_u64_span");
            record!("k", 123u64);
            black_box(());
        });
        Profiler::clear();
    });

    // Record overhead: span + record str (allocates)
    group.bench_function("record_str", |b| {
        b.iter(|| {
            let _g = span!("record_str_span");
            record!("k", "some-key");
            black_box(());
        });
        Profiler::clear();
    });

    // Record overhead: span + record bytes (allocates)
    group.bench_function("record_bytes", |b| {
        let bytes = [0u8; 32];
        b.iter(|| {
            let _g = span!("record_bytes_span");
            record!("k", &bytes[..]);
            black_box(());
        });
        Profiler::clear();
    });

    // Record overhead: many records in one span
    group.bench_function("record_1k_u64", |b| {
        b.iter(|| {
            let _g = span!("record_1k_u64_span");
            for i in 0..1000u64 {
                record!("k", i);
            }
            black_box(());
        });
        Profiler::clear();
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = bench_overhead, bench_10m_sampled_spans, bench_record
}
criterion_main!(benches);
