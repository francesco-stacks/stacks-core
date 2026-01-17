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

/// Small metadata attached to spans
#[derive(Debug, Clone)]
pub enum RecordValue {
    U64(u64),
    I64(i64),
    Str(Box<str>),
    Bytes(Box<[u8]>),
}

impl From<u64> for RecordValue {
    #[inline(always)]
    fn from(v: u64) -> Self { RecordValue::U64(v) }
}
impl From<i64> for RecordValue {
    #[inline(always)]
    fn from(v: i64) -> Self { RecordValue::I64(v) }
}
impl From<&str> for RecordValue {
    #[inline(always)]
    fn from(v: &str) -> Self { RecordValue::Str(v.into()) }
}
impl From<String> for RecordValue {
    #[inline(always)]
    fn from(v: String) -> Self { RecordValue::Str(v.into_boxed_str()) }
}
impl From<&[u8]> for RecordValue {
    #[inline(always)]
    fn from(v: &[u8]) -> Self { RecordValue::Bytes(v.into()) }
}

/// Key/value record attached to a span node.
#[derive(Debug, Clone)]
pub struct Record {
    pub key: &'static str,
    pub value: RecordValue,
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
    pub entered_count: usize,
    pub sampled_count: usize,
    pub records: Vec<Record>,
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
        self.entered_count
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
        runtime::begin_span_timed(id, tag);
        ProfileGuard {
            kind: GuardKind::Span,
        }
    }

    /// Count-only: preserves hierarchy + increments counts, but does not read clocks.
    #[inline(always)]
    pub fn begin_span_count_only(id: &'static SpanId, tag: Option<Tag>) -> ProfileGuard {
        runtime::begin_span_count_only(id, tag);
        ProfileGuard {
            kind: GuardKind::Span,
        }
    }

    /// Enter a suppression region: nested spans become no-ops (prevent wrong-parent attachment).
    #[inline(always)]
    pub fn begin_suppression() -> ProfileGuard {
        runtime::begin_suppression();
        ProfileGuard {
            kind: GuardKind::Suppression,
        }
    }

    #[inline]
    #[doc(hidden)]
    pub fn end_span() {
        runtime::end_span();
    }

    #[doc(hidden)]
    #[inline(always)]
    pub fn is_suppressed() -> bool {
        runtime::is_suppressed()
    }

    #[inline(always)]
    #[doc(hidden)]
    pub fn end_suppression() {
        runtime::end_suppression();
    }

    #[inline(always)]
    pub fn record(key: &'static str, value: RecordValue) {
        runtime::record_kv(key, value);
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

enum GuardKind {
    /// Timed or count-only; should call `end_span()` on `drop()`.
    Span,
    /// Only affects suppression depth; should call `end_suppression()` on `drop()`.
    Suppression,
}

pub struct ProfileGuard {
    kind: GuardKind,
}

impl Drop for ProfileGuard {
    #[inline]
    fn drop(&mut self) {
        match self.kind {
            GuardKind::Span => Profiler::end_span(),
            GuardKind::Suppression => Profiler::end_suppression(),
        }
    }
}
