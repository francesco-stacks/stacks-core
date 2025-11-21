use stacks_profiler::{profile, profile_scope, Profiler};
use std::panic;
use std::thread;
use std::time::Duration;

/// Helper to ensure we start with a clean state in tests
fn clear_profiler() {
    let _ = Profiler::take_results();
}

#[test]
fn test_basic_nesting() {
    clear_profiler();

    profile_scope!("Root", {
        thread::sleep(Duration::from_millis(1));
        profile_scope!("Child", {
            thread::sleep(Duration::from_millis(1));
        });
    });

    let results = Profiler::take_results();
    
    assert_eq!(results.len(), 1, "Should have 1 root");
    let root = &results[0];
    assert_eq!(root.id.name, "Root");
    
    assert_eq!(root.children.len(), 1, "Root should have 1 child");
    let child = &root.children[0];
    assert_eq!(child.id.name, "Child");
}

#[test]
fn test_macro_variations() {
    clear_profiler();

    // 1. Statement style (wrapped in block to force drop)
    {
        profile_scope!("Statement");
    } 

    // 2. Block style
    profile_scope! {
        let _x = 1 + 1;
    }; 

    // 3. Expression style
    let res = profile_scope!("Expression", {
        5 + 5
    });
    assert_eq!(res, 10);

    let results = Profiler::take_results();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].name(), "Statement");
    assert_eq!(results[1].name(), "scope");
    assert_eq!(results[2].name(), "Expression");
}

#[test]
fn test_multi_threading_isolation() {
    clear_profiler();

    // Spawn a thread that does profiling
    let t = thread::spawn(|| {
        // Wrap in block so guard drops BEFORE take_results
        {
            profile_scope!("ThreadWork");
            thread::sleep(Duration::from_millis(10));
        }
        // Return the results to the main thread
        Profiler::take_results()
    });

    // Do work on main thread simultaneously
    {
        profile_scope!("MainWork");
        thread::sleep(Duration::from_millis(10));
    } // Drops here, finishing the span

    let thread_results = t.join().expect("Thread failed");
    let main_results = Profiler::take_results();

    // Verify Thread Results
    assert_eq!(thread_results.len(), 1, "Thread should have 1 result");
    assert_eq!(thread_results[0].name(), "ThreadWork");

    // Verify Main Results
    assert_eq!(main_results.len(), 1, "Main thread should have 1 result");
    assert_eq!(main_results[0].name(), "MainWork");

    // Ensure no cross-contamination
    assert!(!main_results.iter().any(|r| r.name() == "ThreadWork"));
}

#[test]
fn test_panic_safety() {
    clear_profiler();

    let result = panic::catch_unwind(|| {
        profile_scope!("WillPanic");
        panic!("Oops");
    });
    assert!(result.is_err());

    // Run a normal profile to prove the stack recovered
    {
        profile_scope!("Recovered");
    } // Drops here

    let results = Profiler::take_results();

    // Logic:
    // 1. "WillPanic" started.
    // 2. Panic -> stack unwind -> guard dropped -> "WillPanic" finished & recorded.
    // 3. "Recovered" started -> finished -> recorded.
    
    assert_eq!(results.len(), 2, "Should have 'WillPanic' and 'Recovered'");
    assert_eq!(results[0].name(), "WillPanic");
    assert_eq!(results[1].name(), "Recovered");
}

#[test]
fn test_recursion() {
    clear_profiler();

    #[profile(name="Recursive")]
    fn recursive_func(depth: usize) {
        if depth > 0 {
            recursive_func(depth - 1);
        }
    }

    recursive_func(3);

    let results = Profiler::take_results();
    assert_eq!(results.len(), 1);
    
    let mut current = &results[0];
    let mut depth = 0;
    assert_eq!(current.name(), "Recursive");

    while !current.children.is_empty() {
        current = &current.children[0];
        assert_eq!(current.name(), "Recursive");
        depth += 1;
    }
    
    assert_eq!(depth, 3);
}

#[test]
fn test_zero_time_safety() {
    clear_profiler();
    
    // Ensure very fast operations don't cause underflow/crashes
    for _ in 0..1000 {
        profile_scope!("Fast");
    }

    let results = Profiler::take_results();
    
    // Because all calls happen at the same file/line, they are aggregated.
    assert_eq!(results.len(), 1, "Should aggregate 1000 identical calls into 1 entry");
    assert_eq!(results[0].count, 1000, "Count should reflect the loop iterations");
    assert_eq!(results[0].id.name, "Fast");
}
