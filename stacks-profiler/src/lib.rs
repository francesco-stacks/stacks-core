//! # Stacks Profiler
//!
//! A high-performance, allocation-optimized profiler using Thread Local Storage.

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
// Platform Specific CPU Timer
// ==============================================================

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    #[inline(always)]
    pub fn thread_cpu_nanos() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
        }
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    #[inline(always)]
    pub fn thread_cpu_nanos() -> u64 {
        0
    }
}

/// A lightweight tag for distinguishing spans with the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tag {
    U64(u64),
    I64(i64),
    Usize(usize),
    Str(&'static str),
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tag::U64(v) => write!(f, "{}", v),
            Tag::I64(v) => write!(f, "{}", v),
            Tag::Usize(v) => write!(f, "{}", v),
            Tag::Str(v) => write!(f, "{}", v),
        }
    }
}

impl From<u64> for Tag {
    #[inline]
    fn from(v: u64) -> Self {
        Tag::U64(v)
    }
}

impl From<i64> for Tag {
    #[inline]
    fn from(v: i64) -> Self {
        Tag::I64(v)
    }
}

impl From<u32> for Tag {
    #[inline]
    fn from(v: u32) -> Self {
        Tag::U64(v as u64)
    }
}

impl From<i32> for Tag {
    #[inline]
    fn from(v: i32) -> Self {
        Tag::I64(v as i64)
    }
}

impl From<usize> for Tag {
    #[inline]
    fn from(v: usize) -> Self {
        Tag::Usize(v)
    }
}

/// Identifies a specific span of execution.
#[derive(Debug, Copy, Clone, Eq, Hash)]
pub struct SpanId {
    pub name: &'static str,
    pub context: Option<&'static str>,
    pub file: &'static str,
    pub line: u32,
}

impl PartialEq for SpanId {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        // Optimization: Pointer equality check first.
        if std::ptr::eq(self.name, other.name)
            && std::ptr::eq(self.file, other.file)
            && self.line == other.line
        {
            match (self.context, other.context) {
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                (None, None) => true,
                _ => false,
            }
        } else {
            self.name == other.name
                && self.file == other.file
                && self.line == other.line
                && self.context == other.context
        }
    }
}

impl SpanId {
    #[inline(always)]
    fn new_from_loc(name: &'static str, loc: &'static Location) -> Self {
        Self {
            name,
            context: None,
            file: loc.file(),
            line: loc.line(),
        }
    }

    #[inline(always)]
    pub fn with_context(mut self, context: &'static str) -> Self {
        self.context = Some(context);
        self
    }
}

/// Represents the collected metrics for a specific span of execution.
#[derive(Debug, Clone)]
pub struct ProfileStats {
    pub id: &'static SpanId,
    pub tag: Option<Tag>,
    pub wall_time_ns: u64,
    pub cpu_time_ns: u64,
    pub children: Vec<ProfileStats>,
    pub count: usize,
}

impl ProfileStats {
    pub fn name(&self) -> &'static str {
        self.id.name
    }

    pub fn context(&self) -> Option<&'static str> {
        self.id.context
    }

    pub fn source_file(&self) -> &'static str {
        self.id.file
    }

    pub fn source_line(&self) -> u32 {
        self.id.line
    }

    pub fn tag(&self) -> Option<&Tag> {
        self.tag.as_ref()
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Calculates the time the thread was suspended.
    pub fn wait_time(&self) -> Duration {
        Duration::from_nanos(self.wall_time_ns.saturating_sub(self.cpu_time_ns))
    }

    pub fn wall_time(&self) -> Duration {
        Duration::from_nanos(self.wall_time_ns)
    }

    pub fn cpu_time(&self) -> Duration {
        Duration::from_nanos(self.cpu_time_ns)
    }

    pub fn wait_time_ns(&self) -> u64 {
        self.wall_time_ns.saturating_sub(self.cpu_time_ns)
    }

    pub fn wall_time_micros(&self) -> u64 {
        self.wall_time_ns / 1_000
    }

    pub fn cpu_time_micros(&self) -> u64 {
        self.cpu_time_ns / 1_000
    }

    /// Merges another stats object into this one.
    /// Returns the `Vec` from `other` so it can be recycled.
    #[must_use]
    #[inline]
    fn merge(&mut self, mut other: ProfileStats) -> Vec<ProfileStats> {
        self.wall_time_ns += other.wall_time_ns;
        self.cpu_time_ns += other.cpu_time_ns;
        self.count += other.count;

        // Optimization: Vector Stealing
        if self.children.is_empty() {
            // We take 'other.children' entirely.
            self.children = other.children;
            // We return a new empty vec (0 capacity, no allocation)
            // because we can't return the one we just stole.
            return Vec::new();
        } else if !other.children.is_empty() {
            // FIX: Use drain(..)
            // This yields the owned items one by one, but keeps the 'other.children'
            // vector alive (and empty) with its capacity preserved.
            for other_child in other.children.drain(..) {
                // Note: The recursive recycled vectors returned here are currently dropped.
                // To support deep recycling, we'd need access to the thread-local pool here,
                // but that might be overkill. Recycling the top-level vector is the biggest win.
                let _ = Self::merge_into_list(&mut self.children, other_child);
            }
        }

        // 'other.children' is now empty (because of drain) but still has its capacity.
        // We return it to the caller to be put back into the pool.
        other.children
    }

    #[inline]
    fn merge_into_list(
        list: &mut Vec<ProfileStats>,
        stats: ProfileStats,
    ) -> Option<Vec<ProfileStats>> {
        // Try last sibling (fast path for loops)
        if let Some(last) = list.last_mut() {
            if last.id == stats.id && last.tag == stats.tag {
                return Some(last.merge(stats));
            }
        }

        // Linear reverse search (slower path)
        if let Some(existing) = list
            .iter_mut()
            .rev()
            .find(|c| c.id == stats.id && c.tag == stats.tag)
        {
            return Some(existing.merge(stats));
        }

        // New entry
        list.push(stats);
        None
    }

    // ========================================================================
    // Printing Logic
    // ========================================================================

    /// Recursively prints the profiling tree to stdout.
    pub fn print_tree(&self) {
        // 1. Root Header
        self.print_node_header("", "", true);

        // 2. Recurse (No footer needed for root in standard tree view)
        self.print_children_recursive("");
    }

    /// Internal helper to iterate children
    fn print_children_recursive(&self, prefix: &str) {
        let len = self.children.len();
        for (i, child) in self.children.iter().enumerate() {
            let is_last = i == len - 1;

            // 1. Determine Connector
            // Last sibling gets "└── ", others get "├── "
            let connector = if is_last { "└── " } else { "├── " };

            // 2. Print Header
            child.print_node_header(prefix, connector, false);

            // 3. Determine Prefix for Grandchildren
            // If this was the last sibling, the vertical spine stops here ("    ").
            // Otherwise, it continues ("│   ").
            let child_prefix_segment = if is_last { "    " } else { "│   " };
            // We build the prefix as Plain Text to avoid color artifacting
            let child_prefix = format!("{}{}", prefix, child_prefix_segment);

            // 4. Recurse
            child.print_children_recursive(&child_prefix);

            // 5. No Footer. The "└── " from step 1 visually closed this block.
        }
    }

    /// Prints the "Header" line: Connector + Name + File + Metrics
    fn print_node_header(&self, prefix: &str, connector: &str, is_root: bool) {
        let reset = Style::RESET;
        let gray = Style::GRAY;
        let bold = Style::BOLD;
        let cyan = Style::CYAN;
        let dim = Style::DIM;
        let white = Style::WHITE;

        let name_icon = if is_root { "" } else { "▶" };
        let name = self.id.name;
        let file = self.id.file;
        let line = self.id.line;

        // Format Tag
        let tag_display = if let Some(t) = self.tag {
            match t {
                Tag::U64(v) => format!(" {cyan}#{v}{reset}"),
                Tag::I64(v) => format!(" {cyan}#{v}{reset}"),
                Tag::Usize(v) => format!(" {cyan}#{v}{reset}"),
                Tag::Str(v) => format!(" {cyan}[{v}]{reset}"),
            }
        } else {
            String::new()
        };

        let metrics = self.format_metrics();

        let source_loc = format!("{reset}{dim}{gray}@ {file}:{line}{reset}");

        if is_root {
            print!("{bold}{white}{name}{tag_display} {metrics} {source_loc}");
        } else {
            print!(
                "{gray}{prefix}{connector}{reset}{gray}{name_icon}{reset} {bold}{white}{name}{tag_display} {metrics} {source_loc}"
            );
        }
        println!();
    }

    /// Generates the formatted metrics string
    fn format_metrics(&self) -> String {
        let reset = Style::RESET;
        let gray = Style::GRAY;
        let red = Style::RED;
        let cyan = Style::CYAN;
        let dim = Style::DIM;
        //let metrics_icon = ""; //∫

        // Use u64 fields
        let wall_ms = self.wall_time_ns as f64 / 1_000_000.0;
        let cpu_ms = self.cpu_time_ns as f64 / 1_000_000.0;
        let wait_ns = self.wait_time_ns();
        let wait_ms = wait_ns as f64 / 1_000_000.0;

        let wait_color = if wait_ns > self.cpu_time_ns {
            red
        } else {
            gray
        };
        let count = self.count;

        format!(
            "{reset}{dim}[total: {cyan}{wall_ms:.3}ms {reset}{dim}| busy: {cyan}{cpu_ms:.3}ms{reset} {dim}| wait: {reset}{wait_color}{wait_ms:.3}ms{reset}{dim}]{reset} {gray}(x{count}){reset}"
        )
    }
}

// -----------------------------------------------------------------------------
// OPTIMIZED INTERNAL STATE
// -----------------------------------------------------------------------------

struct ActiveSpan {
    /// Zero-copy reference to the static ID.
    id: &'static SpanId,
    tag: Option<Tag>,
    start_wall: Instant,
    start_cpu_ns: u64,
    /// We use `Option` so we can take() it out without moving the struct.
    children: Option<Vec<ProfileStats>>,
}

struct ThreadState {
    active_stack: Vec<ActiveSpan>,
    completed_roots: Vec<ProfileStats>,
    /// Pool of reusable vectors to prevent allocator churn.
    vec_pool: Vec<Vec<ProfileStats>>,
}

thread_local! {
    static STATE: RefCell<ThreadState> = RefCell::new(ThreadState {
        active_stack: Vec::with_capacity(32),
        completed_roots: Vec::with_capacity(4),
        vec_pool: Vec::with_capacity(16),
    });
}

// -----------------------------------------------------------------------------
// PROFILER API
// -----------------------------------------------------------------------------

pub struct Profiler;

impl Profiler {
    #[inline]
    #[track_caller]
    pub fn new_span_id(name: &'static str) -> SpanId {
        let loc = Location::caller();
        SpanId::new_from_loc(name, loc)
    }

    /// Starts a new profiling span.
    ///
    /// Requires a `&'static SpanId`. Use the `profile_scope!` macro to generate these safely.
    #[inline]
    pub fn begin_span(id: &'static SpanId, tag: Option<Tag>) -> ProfileGuard {
        let start_wall = Instant::now();
        let start_cpu_ns = platform::thread_cpu_nanos();

        STATE.with(|cell| {
            let mut state = cell.borrow_mut();

            state.active_stack.push(ActiveSpan {
                id,
                tag,
                start_wall,
                start_cpu_ns,
                children: None,
            });
        });

        ProfileGuard
    }

    #[inline]
    #[doc(hidden)]
    pub fn end_span() {
        let end_wall = Instant::now();
        let end_cpu_ns = platform::thread_cpu_nanos();

        STATE.with(|cell| {
            let mut state = cell.borrow_mut();

            // Split borrow
            let ThreadState {
                active_stack,
                completed_roots,
                vec_pool,
            } = &mut *state;

            if let Some(mut active) = active_stack.pop() {
                let wall_ns = end_wall.duration_since(active.start_wall).as_nanos() as u64;
                let cpu_ns = end_cpu_ns.saturating_sub(active.start_cpu_ns);

                // Recover children or grab a recycled vector
                let children = active
                    .children
                    .take()
                    .unwrap_or_else(|| vec_pool.pop().unwrap_or_default());

                let stats = ProfileStats {
                    id: active.id,
                    tag: active.tag,
                    wall_time_ns: wall_ns,
                    cpu_time_ns: cpu_ns,
                    children,
                    count: 1,
                };

                let recycled_vec = if let Some(parent) = active_stack.last_mut() {
                    // Lazy allocation for parent's children list
                    let list = parent
                        .children
                        .get_or_insert_with(|| vec_pool.pop().unwrap_or_default());

                    // Merge and potentially get back a recycled vector
                    ProfileStats::merge_into_list(list, stats)
                } else {
                    ProfileStats::merge_into_list(completed_roots, stats)
                };

                // If we got a vector back from merging, put it in the pool
                if let Some(vec) = recycled_vec {
                    if vec_pool.len() < 1024 {
                        // Sanity cap
                        vec_pool.push(vec);
                    }
                }
            }
        });
    }

    #[inline]
    pub fn take_results() -> Vec<ProfileStats> {
        STATE.with(|cell| std::mem::take(&mut cell.borrow_mut().completed_roots))
    }

    #[inline]
    pub fn clear() {
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.completed_roots.clear();
            state.active_stack.clear();
            // We can keep the vec_pool as is, or clear it if we want to release memory
        });
    }
}

pub struct ProfileGuard;

impl Drop for ProfileGuard {
    #[inline]
    fn drop(&mut self) {
        Profiler::end_span();
    }
}

// ========================================================================
// Macros
// ========================================================================

#[macro_export]
macro_rules! measure {
    // Tagged & Block: measure!("Name", tag_expr, { ... })
    ($name:literal, $tag:expr, $block:block) => {
        {
            static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
            let __profiler_span_id_ref = __PROFILER_SPAN_ID.get_or_init(|| {
                $crate::Profiler::new_span_id($name).with_context(module_path!())
            });
            let __tag = Into::into($tag);
            let _guard = $crate::Profiler::begin_span(__profiler_span_id_ref, Some(__tag));
            $block
        }
    };

    // Named Block (No Tag): measure!("Name", { ... })
    ($name:literal, $block:block) => {
        {
            static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
            let __profiler_span_id_ref = __PROFILER_SPAN_ID.get_or_init(|| {
                $crate::Profiler::new_span_id($name).with_context(module_path!())
            });
            let _guard = $crate::Profiler::begin_span(__profiler_span_id_ref, None);
            $block
        }
    };

    // Trap
    ($name:literal) => {
        let _guard = {
            static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
            let id = __PROFILER_SPAN_ID.get_or_init(|| {
                $crate::Profiler::new_span_id($name).with_context(module_path!())
            });
            $crate::Profiler::begin_span(id, None)
        };
    };

    // Anonymous Block
    ($($t:tt)*) => {
        {
            static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
            let __profiler_span_id_ref = __PROFILER_SPAN_ID.get_or_init(|| {
                $crate::Profiler::new_span_id("scope").with_context(module_path!())
            });
            let _guard = $crate::Profiler::begin_span(__profiler_span_id_ref, None);
            $($t)*
        }
    };
}

#[macro_export]
macro_rules! span {
    // Tagged Guard: span!("Name", tag_expr)
    ($name:literal, $tag:expr) => {{
        static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
        let id = __PROFILER_SPAN_ID
            .get_or_init(|| $crate::Profiler::new_span_id($name).with_context(module_path!()));
        let tag = Into::into($tag);
        $crate::Profiler::begin_span(id, Some(tag))
    }};

    // Standard Guard: span!("Name")
    ($name:literal) => {{
        static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
        let id = __PROFILER_SPAN_ID
            .get_or_init(|| $crate::Profiler::new_span_id($name).with_context(module_path!()));
        $crate::Profiler::begin_span(id, None)
    }};
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

        // We use a stack of references to traverse without moving the originals
        let mut stack: Vec<&ProfileStats> = self.iter().collect();

        while let Some(node) = stack.pop() {
            // 1. Merge this node into the global map.
            // We must Clone the node because 'flatten' creates a new aggregated view
            // separate from the original tree.
            map.entry(node.id.clone())
                .and_modify(|existing| {
                    // We ignore the returned recycling vec because we are in analysis mode
                    let _ = existing.merge(node.clone());
                })
                .or_insert_with(|| node.clone());

            // 2. Add children to stack to visit them as well.
            // This ensures that children also become top-level entries in the flat list.
            for child in &node.children {
                stack.push(child);
            }
        }

        // Convert to sorted vec
        let mut flat_results: Vec<ProfileStats> = map.into_values().collect();
        // Sort by Wall Time descending
        flat_results.sort_by(|a, b| b.wall_time_ns.cmp(&a.wall_time_ns));
        flat_results
    }
}

impl ProfileAnalyzer for ProfileStats {
    fn flatten(&self) -> Vec<ProfileStats> {
        // Treat this single node as a root of a tree and flatten it.
        vec![self.clone()].flatten()
    }
}
