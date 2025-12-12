//! # Stacks Profiler
//!
//! A high-performance, allocation-optimized profiler using Thread Local Storage.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::Location;
use std::time::{Duration, Instant};

// Re-export the profiling procedural macro
pub use stacks_profiler_macros::profile;

pub mod print;

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

impl From<u64> for Tag {
    #[inline(always)]
    fn from(v: u64) -> Self {
        Tag::U64(v)
    }
}

impl From<i64> for Tag {
    #[inline(always)]
    fn from(v: i64) -> Self {
        Tag::I64(v)
    }
}

impl From<u32> for Tag {
    #[inline(always)]
    fn from(v: u32) -> Self {
        Tag::U64(v as u64)
    }
}

impl From<i32> for Tag {
    #[inline(always)]
    fn from(v: i32) -> Self {
        Tag::I64(v as i64)
    }
}

impl From<usize> for Tag {
    #[inline(always)]
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

    /// Recursively prints the profiling tree to stdout using the default PrettyPrinter.
    pub fn print_tree(&self) {
        crate::print::print_tree(self, &crate::print::PrettyPrinter);
    }

    /// Prints the tree using a custom formatter.
    pub fn print_with<F: crate::print::TreeFormatter>(&self, formatter: &F) {
        crate::print::print_tree(self, formatter);
    }
}

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
    #[inline(always)]
    #[track_caller]
    pub fn new_span_id(name: &'static str) -> SpanId {
        let loc = Location::caller();
        SpanId::new_from_loc(name, loc)
    }

    /// Starts a new profiling span.
    ///
    /// Requires a `&'static SpanId`. Use the `profile_scope!` macro to generate these safely.
    #[inline(always)]
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
    // Name, Tag, Rate, Block
    ($name:literal, $tag:expr, rate: $rate:literal, $block:block) => {
        {
            let _guard = $crate::span!($name, $tag, rate: $rate);
            $block
        }
    };

    // Name, Rate, Block
    ($name:literal, rate: $rate:literal, $block:block) => {
        {
            let _guard = $crate::span!($name, rate: $rate);
            $block
        }
    };

    // Name, Tag, Block
    ($name:literal, $tag:expr, $block:block) => {
        {
            let _guard = $crate::span!($name, $tag);
            $block
        }
    };

    // Name, Block
    ($name:literal, $block:block) => {
        {
            let _guard = $crate::span!($name);
            $block
        }
    };

    // Trap (Name, Rate)
    ($name:literal, rate: $rate:literal) => {
        let _guard = $crate::span!($name, rate: $rate);
    };

    // Trap (Name)
    ($name:literal) => {
        let _guard = $crate::span!($name);
    };

    // Anonymous Block
    ($($t:tt)*) => {
        {
            let _guard = $crate::span!("scope");
            $($t)*
        }
    };
}

#[macro_export]
macro_rules! span {
    // Internal helpers

    (@get_id $name:literal) => {{
        static __PROFILER_SPAN_ID: std::sync::OnceLock<$crate::SpanId> = std::sync::OnceLock::new();
        __PROFILER_SPAN_ID.get_or_init(|| $crate::Profiler::new_span_id($name).with_context(module_path!()))
    }};

    (@begin $id:expr, $tag_opt:expr) => {{
        Some($crate::Profiler::begin_span($id, $tag_opt))
    }};

    (@should_sample $counter:ident, $rate:literal) => {{
        const __RATE: usize = $rate;
        if __RATE <= 1 {
            true
        } else {
            let __n = $counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // Fast-path for power-of-two rates: n % rate == 0 <=> (n & (rate-1)) == 0
            if __RATE.is_power_of_two() {
                (__n & (__RATE - 1)) == 0
            } else {
                (__n % __RATE) == 0
            }
        }
    }};

    (@sampled $counter:ident, $rate:literal, $sampled_block:block) => {{
        if $crate::span!(@should_sample $counter, $rate) {
            $sampled_block
        } else {
            None
        }
    }};

    // Public forms

    // Name, Tag, Rate
    ($name:literal, $tag:expr, rate: $rate:literal) => {{
        static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);

        $crate::span!(@sampled __PROFILER_SAMPLE_COUNTER, $rate, {
            let __id = $crate::span!(@get_id $name);
            // Only convert the tag when we actually sample.
            let __tag: $crate::Tag = ::core::convert::Into::into($tag);
            $crate::span!(@begin __id, Some(__tag))
        })
    }};

    // Name, Rate
    ($name:literal, rate: $rate:literal) => {{
        static __PROFILER_SAMPLE_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);

        $crate::span!(@sampled __PROFILER_SAMPLE_COUNTER, $rate, {
            let __id = $crate::span!(@get_id $name);
            $crate::span!(@begin __id, None)
        })
    }};

    // Name, Tag
    ($name:literal, $tag:expr) => {{
        let __id = $crate::span!(@get_id $name);
        let __tag: $crate::Tag = ::core::convert::Into::into($tag);
        $crate::span!(@begin __id, Some(__tag))
    }};

    // Name
    ($name:literal) => {{
        let __id = $crate::span!(@get_id $name);
        $crate::span!(@begin __id, None)
    }};
}

/// Trait for analyzing and flattening profiling trees.
pub trait Flatten {
    /// Flattens the hierarchy into a list, aggregating stats by SpanId.
    ///
    /// This transforms a Tree View into a Flat/Bottom-Up View.
    /// * **Tree View:** Preserves context (e.g., "Tx1 -> Logic" is separate from "Tx2 -> Logic").
    /// * **Flat View:** Aggregates by function (e.g., "Logic" shows total time across all Txs).
    ///
    /// The resulting list is sorted by Wall Time (descending).
    fn flatten(&self) -> Vec<ProfileStats>;
}

impl Flatten for Vec<ProfileStats> {
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

impl Flatten for ProfileStats {
    fn flatten(&self) -> Vec<ProfileStats> {
        // Treat this single node as a root of a tree and flatten it.
        vec![self.clone()].flatten()
    }
}
