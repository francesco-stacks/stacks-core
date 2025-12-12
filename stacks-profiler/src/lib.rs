//! # Stacks Profiler
//!
//! A high-performance, allocation-optimized profiler using Thread Local Storage.

use std::panic::Location;
use std::time::Duration;

// Re-export the profiling procedural macro
pub use stacks_profiler_macros::profile;

mod macros;
mod platform;
mod runtime;

pub mod print;
pub mod util;

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
        runtime::begin_span(id, tag);
        ProfileGuard
    }

    #[inline]
    #[doc(hidden)]
    pub fn end_span() {
        runtime::end_span();
    }

    #[inline]
    pub fn take_results() -> Vec<ProfileStats> {
        runtime::take_results()
    }

    #[inline]
    pub fn clear() {
        runtime::clear();
    }
}

pub struct ProfileGuard;

impl Drop for ProfileGuard {
    #[inline]
    fn drop(&mut self) {
        Profiler::end_span();
    }
}
