//! # Stacks Profiler
//!
//! `stacks-profiler` provides high-resolution tracing of CPU time versus Wall time.
//! By comparing these two metrics, it calculates "Wait Time"—the time a thread spent
//! suspended (sleeping, waiting for I/O, or waiting on locks).
//!
//! ## Architecture
//! This crate relies on **Thread Local Storage (TLS)**.
//! * **Pros:** Zero lock contention, extremely low overhead, safe for parallel code.
//! * **Cons:** You must call `Profiler::take_results()` on *each* thread you wish to measure.
//!
//! ## Asynchronous Caveat
//! Because this uses TLS, it is **not** safe to carry a span across `await` points in a
//! multi-threaded runtime (like Tokio's multi-threaded scheduler), as the task may move
//! between threads. It is safe for synchronous code and `current_thread` runtimes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::Location;
use std::time::{Duration, Instant};

// Re-export the profiling procedural macro
pub use stacks_profiler_macros::profile;

struct Style;

#[allow(unused)]
impl Style {
    const RESET: &str = "\x1b[0m";
    const BOLD: &str = "\x1b[1m";
    const GRAY: &str = "\x1b[90m";
    const RED: &str = "\x1b[31m";
    const DIM: &str = "\x1b[2m";
    const GREEN: &str = "\x1b[32m";
    const YELLOW: &str = "\x1b[33m";
    const CYAN: &str = "\x1b[36m";
    const BLUE: &str = "\x1b[34m";
    const WHITE: &str = "\x1b[37m";
}

// ==============================================================
// Platform Specific CPU Timer (Unified for Linux & macOS)
// ==============================================================

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use std::time::Duration;

    #[inline]
    pub fn thread_cpu_time() -> Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            // CLOCK_THREAD_CPUTIME_ID is supported on Linux and macOS (>= 10.12).
            // On macOS, libc maps this to the correct constant (16).
            libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
        }
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::time::Duration;

    #[inline]
    pub fn thread_cpu_time() -> Duration {
        Duration::ZERO // Fallback for Windows
    }
}

// ==============================================================
// Profiling Logic
// ==============================================================

/// Identifies a specific span of execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpanId {
    /// The name of the span (from `#[profile]`, [`profile_scope!`] or [`Profiler::begin_span`]).
    pub name: &'static str,
    /// The source file where the span was defined.
    pub file: &'static str,
    /// The line number in the source file.
    pub line: u32,
}

impl SpanId {
    #[inline]
    fn new_from_loc(name: &'static str, loc: &'static Location) -> Self {
        Self {
            name,
            file: loc.file(),
            line: loc.line(),
        }
    }
}

/// Represents the collected metrics for a specific span of execution.
#[derive(Debug, Clone)]
pub struct ProfileStats {
    /// The identifier for this span.
    pub id: SpanId,
    /// Total real-world time elapsed.
    pub wall_time: Duration,
    /// Total CPU time consumed by the thread during this span.
    pub cpu_time: Duration,
    /// Child spans called within this span.
    pub children: Vec<ProfileStats>,
    /// Number of times this span was called (currently always 1 for trace mode).
    pub count: usize,
}

impl ProfileStats {
    /// Calculates the time the thread was suspended.
    ///
    /// Formula: `max(0, wall_time - cpu_time)`
    pub fn wait_time(&self) -> Duration {
        self.wall_time.saturating_sub(self.cpu_time)
    }

    /// Merges another stats object into this one.
    /// Used to aggregate loops and repeated calls.
    #[inline]
    fn merge(&mut self, other: ProfileStats) {
        self.wall_time += other.wall_time;
        self.cpu_time += other.cpu_time;
        self.count += other.count;

        // OPTIMIZATION: When merging children, we also use the "Last Sibling" check
        // to speed up merging of deep trees.
        for other_child in other.children {
            Self::merge_into_list(&mut self.children, other_child);
        }
    }

    /// Optimized merge logic
    #[inline]
    fn merge_into_list(list: &mut Vec<ProfileStats>, stats: ProfileStats) {
        // Check the last element first. This makes tight loops O(1) instead of O(N).
        if let Some(last) = list.last_mut() {
            if last.id == stats.id {
                last.merge(stats);
                return;
            }
        }

        // Fallback to linear search for non-sequential repeats and iterate backwards because repeats are likely recent.
        if let Some(existing) = list.iter_mut().rev().find(|c| c.id == stats.id) {
            existing.merge(stats);
        } else {
            list.push(stats);
        }
    }

    /// Recursively prints the profiling tree to stdout.
    pub fn print_tree(&self) {
        // 1. Root Header
        self.print_node_header("", "", self, true);

        // 2. Recurse Children
        self.print_children_recursive("");

        // 3. Root Metrics
        // Uses GRAY (90m) instead of DIM (2m) for consistency
        let gray = Style::GRAY;
        let dim = Style::DIM;
        let reset = Style::RESET;

        println!("{gray}{dim}└ {reset}{}{reset}", self.format_metrics());
    }

    /// Internal helper to iterate children
    fn print_children_recursive(&self, prefix: &str) {
        // Use GRAY for the tree structure to match file paths/icons
        //let gray = Style::GRAY;
        let dim = Style::DIM;
        //let red = Style::RED;
        let reset = Style::RESET;

        for child in &self.children {
            // 1. Header
            // We pass PLAIN TEXT characters here.
            // We will colorize the entire prefix+connector string in print_node_header
            let header_connector = format!("{dim}├ {reset}");
            child.print_node_header(prefix, &header_connector, child, false);

            // 2. Recurse
            // Build the plain text prefix for the next level
            let child_prefix = format!("{dim}{prefix}│ ");
            child.print_children_recursive(&child_prefix);

            // 3. Metrics (Last Child)
            // We apply GRAY to the entire structure string at once.
            // This ensures the '│' from the prefix and the '└──' match perfectly.
            //println!("{gray}{dim}{child_prefix}╰ {reset}{}", child.format_metrics());
            //println!("{gray}{dim}{child_prefix}╰ {reset}");
        }
    }

    /// Prints the "Header" line: Connector + Name + File
    fn print_node_header(
        &self,
        prefix: &str,
        connector: &str,
        stats: &ProfileStats,
        is_root: bool,
    ) {
        let reset = Style::RESET;
        let gray = Style::GRAY;
        let bold = Style::BOLD;
        let green = Style::GREEN;
        let dim = Style::DIM;
        let cyan = Style::CYAN;
        let white = Style::WHITE;

        let name_icon = "┝";
        let name = self.id.name;
        let file = self.id.file;
        let line = self.id.line;

        let metrics = stats.format_metrics();

        if is_root {
            // Root has no connector, so just print name/file
            print!(
                "{dim}{green}{name_icon}{reset}{bold}{white} {name}{reset} {metrics} {gray}{dim}@{reset} {cyan}{dim}{file}:{line}{reset}"
            );
        } else {
            // Child:
            // 1. {gray}{prefix}{connector} -> The Tree Structure (Solid Gray)
            // 2. {reset}{gray}{name_icon}  -> The Arrow Icon (Solid Gray)
            // 3. {reset} {bold}{name}      -> The Name (Bold White)
            print!(
                "{gray}{prefix}{connector}{reset}{dim}{green}{name_icon}{reset} {bold}{white}{name}{reset} {metrics}{reset} {cyan}{dim}{file}:{line}{reset}"
            );
        }
        println!();
    }

    /// Generates the formatted metrics string
    fn format_metrics(&self) -> String {
        let wait = self.wait_time();

        let reset = Style::RESET;
        let gray = Style::GRAY;
        let red = Style::RED;
        let dim = Style::DIM;
        let metrics_icon = "";

        let wall_ms = self.wall_time.as_secs_f64() * 1000.0;
        let cpu_ms = self.cpu_time.as_secs_f64() * 1000.0;
        let wait_ms = wait.as_secs_f64() * 1000.0;

        let wait_color = if wait > self.cpu_time { red } else { gray };
        let count = self.count;

        format!(
            "{gray}{metrics_icon}{reset}{gray}- {wall_ms:.3}ms {reset}{dim}[{reset} {gray}busy: {cpu_ms:.3}ms {dim}/{reset} {wait_color}wait: {wait_ms:.3}ms {reset}{dim}]{reset} {gray}x{count}{reset}"
        )
    }

    pub fn name(&self) -> &'static str {
        self.id.name
    }

    pub fn file(&self) -> &'static str {
        self.id.file
    }

    pub fn line(&self) -> u32 {
        self.id.line
    }
}

struct ActiveSpan {
    id: SpanId,
    start_wall: Instant,
    start_cpu: Duration,
    children: Option<Vec<ProfileStats>>,
}

/// Struct to hold thread-local state.
struct ThreadState {
    active_stack: Vec<ActiveSpan>,
    completed_roots: Vec<ProfileStats>,
}

thread_local! {
    static STATE: RefCell<ThreadState> = RefCell::new(ThreadState {
        active_stack: Vec::with_capacity(32),
        completed_roots: Vec::with_capacity(4),
    });
}

/// The global entry point for controlling the profiler.
pub struct Profiler;

impl Profiler {
    /// Starts a new profiling span and returns a RAII guard.
    ///
    /// When the returned `ProfileGuard` is dropped (e.g. when it goes out of scope),
    /// the span is automatically finished and recorded.
    #[inline]
    #[track_caller]
    pub fn begin_span(name: &'static str) -> ProfileGuard {
        let loc = Location::caller();
        let id = SpanId::new_from_loc(name, loc);
        let span = ActiveSpan {
            id,
            start_wall: Instant::now(),
            start_cpu: platform::thread_cpu_time(),
            children: None, // Lazy alloc
        };
        STATE.with(|state| state.borrow_mut().active_stack.push(span));

        // Return the guard
        ProfileGuard
    }

    /// Manually ends the current profiling span.
    ///
    /// Pops the active span from the thread-local stack, calculates durations,
    /// and appends the result to the parent span (or root list).
    ///
    /// It is public so [`ProfileGuard`] can call it, but users should generally
    /// rely on the guard.
    #[inline]
    #[doc(hidden)]
    pub fn end_span() {
        // Capture time BEFORE borrowing TLS to minimize hold time
        let end_wall = Instant::now();
        let end_cpu = platform::thread_cpu_time();

        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let stack = &mut state.active_stack;

            if let Some(active) = stack.pop() {
                let wall = end_wall.duration_since(active.start_wall);
                let cpu = end_cpu.saturating_sub(active.start_cpu);

                // Recover the children vec, or empty if none existed
                let children = active.children.unwrap_or_default();

                let stats = ProfileStats {
                    id: active.id,
                    wall_time: wall,
                    cpu_time: cpu,
                    children,
                    count: 1,
                };

                if let Some(parent) = stack.last_mut() {
                    // Initialize parent children vec if null
                    let list = parent.children.get_or_insert_with(Vec::new);
                    ProfileStats::merge_into_list(list, stats);
                } else {
                    ProfileStats::merge_into_list(&mut state.completed_roots, stats);
                }
            }
        });
    }

    /// Retrieves and clears the profiling results for the current thread.
    ///
    /// This should be called at the end of your benchmark or unit of work.
    /// Returns a list of root-level spans (spans that had no parent).
    #[inline]
    pub fn take_results() -> Vec<ProfileStats> {
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            // Efficiently swap out the vector
            std::mem::take(&mut state.completed_roots)
        })
    }

    /// Clears all profiling data for the current thread.
    #[inline]
    pub fn clear() {
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.completed_roots.clear();
            state.active_stack.clear();
        });
    }
}

/// A RAII guard that calls `Profiler::end_span()` when dropped.
#[must_use = "Profiling spans are dropped immediately if the guard is not assigned to a variable"]
pub struct ProfileGuard;

impl Drop for ProfileGuard {
    #[inline]
    fn drop(&mut self) {
        Profiler::end_span();
    }
}

/// Trait for analyzing and flattening profiling trees.
pub trait ProfileAnalyzer {
    /// Flattens the hierarchy into a list, aggregating stats by SpanId.
    ///
    /// This transforms a Tree View into a Flat/Bottom-Up View.
    /// * **Tree View:** Preserves context (e.g., "Tx1 -> Logic" is separate from "Tx2 -> Logic").
    /// * **Flat View:** Aggregates by function (e.g., "Logic" shows total time across all Txs).
    ///
    /// The resulting list is sorted by Wall Time (descending).
    fn flatten(&self) -> Vec<ProfileStats>;
}

impl ProfileAnalyzer for Vec<ProfileStats> {
    fn flatten(&self) -> Vec<ProfileStats> {
        let mut map: HashMap<SpanId, ProfileStats> = HashMap::new();
        let mut stack: Vec<&ProfileStats> = self.iter().collect();

        while let Some(node) = stack.pop() {
            // 1. Merge this node into the global map
            // We look up by ID (Name + File + Line)
            map.entry(node.id.clone())
                .and_modify(|existing| existing.merge(node.clone()))
                .or_insert_with(|| node.clone());

            // 2. Add children to stack to visit them as well
            // This ensures that children also become top-level entries in the flat list.
            for child in &node.children {
                stack.push(child);
            }
        }

        // Convert to sorted vec
        let mut flat_results: Vec<ProfileStats> = map.into_values().collect();
        flat_results.sort_by(|a, b| b.wall_time.cmp(&a.wall_time));
        flat_results
    }
}

impl ProfileAnalyzer for ProfileStats {
    fn flatten(&self) -> Vec<ProfileStats> {
        // Treat this single node as a root of a tree and flatten it.
        vec![self.clone()].flatten()
    }
}

/// Creates a profiling scope.
///
/// This macro supports three usage patterns to suit different needs:
///
/// # 1. Named Block (Expression)
/// Profiles a specific block of code with a custom name. The block acts as an expression,
/// so it returns the result of the last line.
///
/// ```rust
/// # use stacks_profiler::profile_scope;
/// let result = profile_scope!("Heavy Calculation", {
///     let x = 5;
///     let y = 10;
///     x * y // Returns 50
/// });
/// ```
///
/// # 2. Anonymous Block
/// Profiles a block of code using the default name `"scope"`.
///
/// ```rust
/// # use stacks_profiler::profile_scope;
/// # use std::time::Duration;
/// profile_scope! {
///     std::thread::sleep(Duration::from_millis(10));
/// };
/// ```
///
/// # 3. Statement (Guard)
/// Creates a RAII guard that profiles the remainder of the current scope (function or block).
/// Useful when you don't want to add extra indentation.
///
/// ```rust
/// # use stacks_profiler::profile_scope;
/// fn my_function() {
///     profile_scope!("Remainder of Function");
///     // ... code ...
///     // Span ends here automatically
/// }
/// ```
#[macro_export]
macro_rules! profile_scope {
    // 1. Named Block: profile_scope!("Name", { ... })
    // Usage: let result = profile_scope!("Calculation", { 1 + 1 });
    ($name:expr, $block:block) => {
        {
            let _guard = $crate::Profiler::begin_span($name);
            $block
        }
    };

    // 2. Statement/Guard style: profile_scope!("Name");
    // Usage: fn foo() { profile_scope!("Whole Function"); ... }
    // Matches a single expression (the name).
    ($name:expr) => {
        let _guard = $crate::Profiler::begin_span($name);
    };

    // 3. Anonymous Block / Catch-all: profile_scope! { ... }
    // Usage: profile_scope! { let x = 1; }
    // Captures any remaining tokens and wraps them in a profiled block.
    ($($t:tt)*) => {
        {
            let _guard = $crate::Profiler::begin_span("scope");
            $($t)*
        }
    };
}
