//! # Stacks Profiler
//!
//! A lightweight, low-overhead profiler built on thread-local storage.
//!
//! ## Key concepts
//!
//! - **Span** — a named region of execution.  Spans form a tree: each span is
//!   either a root or a child of the span that was active when it was entered.
//! - **[`SpanId`]** — a static, callsite-unique identifier (name + source
//!   location).
//! - **[`Tag`]** — an optional value that further distinguishes spans with the
//!   same `SpanId` (e.g., a transaction index).
//! - **[`ProfileGuard`]** — an RAII guard returned by [`Profiler::begin_span`].
//!   Dropping the guard ends the span and records elapsed wall and CPU time.
//! - **[`ProfileStats`]** — the collected metrics tree, retrieved via
//!   [`Profiler::take_results`].
//!
//! ## Threading model
//!
//! All state is **thread-local**: each thread maintains its own independent
//! span stack and node arena.  There is no cross-thread synchronisation on the
//! hot path.  The only process-global state is [`Profiler::enable_record`] /
//! [`Profiler::disable_record`], which is an `AtomicBool` kill-switch for
//! record/counter attachment.
//!
//! ## Typical usage
//!
//! Most code should instrument using the [`span!`], [`measure!`], or
//! [`#[profile]`](profile) macros rather than calling [`Profiler`] methods
//! directly.
//!
//! ```rust
//! use stacks_profiler::{measure, Profiler};
//!
//! measure!("my_work", {
//!     // ... timed ...
//! });
//!
//! let results = Profiler::take_results();
//! for root in &results {
//!     root.print_tree();
//! }
//! ```

use std::cell::{Cell, RefCell};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rapidhash::{HashMapExt, RapidHashMap};
/// Re-exported procedural macro — see [`stacks_profiler_macros::profile`].
pub use stacks_profiler_macros::profile;

mod macros;
mod platform;

pub mod print;

/// A dynamically-typed value that can be attached to a span via [`record!`].
#[derive(Debug, Clone)]
pub enum RecordValue {
    U64(u64),
    I64(i64),
    Str(Box<str>),
    Bytes(Box<[u8]>),
}

impl From<u64> for RecordValue {
    #[inline(always)]
    fn from(v: u64) -> Self {
        RecordValue::U64(v)
    }
}
impl From<i64> for RecordValue {
    #[inline(always)]
    fn from(v: i64) -> Self {
        RecordValue::I64(v)
    }
}
impl From<&str> for RecordValue {
    #[inline(always)]
    fn from(v: &str) -> Self {
        RecordValue::Str(v.into())
    }
}
impl From<String> for RecordValue {
    #[inline(always)]
    fn from(v: String) -> Self {
        RecordValue::Str(v.into_boxed_str())
    }
}
impl From<&[u8]> for RecordValue {
    #[inline(always)]
    fn from(v: &[u8]) -> Self {
        RecordValue::Bytes(v.into())
    }
}

/// A key/value record attached to a span via [`record!`] or [`Profiler::record`].
///
/// Records are per-occurrence: each call appends a new entry (they are not
/// aggregated).  Use [`Counter`] for additive metrics.
#[derive(Debug, Clone)]
pub struct Record {
    pub key: &'static str,
    pub value: RecordValue,
}

/// An aggregated counter attached to a span via [`counter_add!`] or [`Profiler::counter_add`].
///
/// Counters with the same key on the same node are summed (saturating).
#[derive(Debug, Clone)]
pub struct Counter {
    pub key: &'static str,
    pub value: u64,
}

thread_local! {
    /// Thread-local string interner for [`Tag::Str`] values created from owned
    /// `String`s.  Strings are leaked once and then reused for the thread's
    /// lifetime, keeping [`Tag`] `Copy` without per-use allocation.
    static TAG_INTERNER: RefCell<RapidHashMap<Box<str>, &'static str>> =
        RefCell::new(RapidHashMap::with_capacity(64));
}

/// Intern a `String` into a `&'static str` via the thread-local interner.
///
/// Repeated calls with the same string content return the same pointer.
/// The leaked memory is bounded by the number of **distinct** strings
/// interned on a given thread.
#[inline]
fn intern_tag_str(s: String) -> &'static str {
    TAG_INTERNER.with(|cell| {
        let mut map = cell.borrow_mut();
        if let Some(&interned) = map.get(s.as_str()) {
            return interned;
        }
        let boxed: Box<str> = s.into_boxed_str();
        let leaked: &'static str = Box::leak(boxed);
        map.insert(leaked.into(), leaked);
        leaked
    })
}

/// A lightweight, `Copy` discriminator for spans that share the same
/// [`SpanId`] but represent distinct logical instances (e.g., different
/// transaction indices within a block).
///
/// Spans with the same `SpanId` but different tags are stored as separate
/// nodes in the profile tree.  Avoid very-high-cardinality tags at hot
/// callsites, as each distinct `(SpanId, Tag)` pair allocates its own node.
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

impl From<&'static str> for Tag {
    #[inline(always)]
    fn from(v: &'static str) -> Self {
        Tag::Str(v)
    }
}

impl From<String> for Tag {
    #[inline(always)]
    fn from(v: String) -> Self {
        Tag::Str(intern_tag_str(v))
    }
}

/// Index into the per-thread node arena (`ThreadState::nodes`).
type NodeId = u32;

/// Process-global toggle for record/counter attachment.
///
/// This is intentionally *not* thread-local: it acts as a single
/// kill-switch that any thread can flip (e.g., to disable verbose
/// recording during a low-overhead warm-up phase).  Span entry, timing,
/// and hierarchy are **unaffected** by this flag.
static RECORD_ENABLED: AtomicBool = AtomicBool::new(true);

/// A single node in the per-thread profile arena.
///
/// Nodes are keyed by `(SpanId pointer, Tag)`.  Multiple entries of the
/// same span under the same parent **share** a node; timing is accumulated.
#[derive(Debug)]
struct Node {
    id: &'static SpanId,
    tag: Option<Tag>,

    wall_time_ns: u64,
    cpu_time_ns: u64,
    entered_count: usize,
    sampled_count: usize,

    children: Vec<NodeId>,
    last_child: Option<NodeId>,

    records: Vec<Record>,
    counters: Vec<Counter>,
}

impl Node {
    /// Returns `true` if this node matches the given `(SpanId, Tag)` key.
    #[inline(always)]
    fn key_eq(&self, id: &'static SpanId, tag: Option<Tag>) -> bool {
        // Fast path: callsite SpanIds are typically pointer-unique.
        std::ptr::eq(self.id, id) && self.tag == tag
    }
}

/// Discriminates timed vs count-only entries on the active stack.
#[derive(Debug)]
enum ActiveKind {
    Timed {
        start_wall: Instant,
        start_cpu_ns: u64,
    },
    CountOnly,
}

/// One frame on the per-thread active-span stack.
#[derive(Debug)]
struct ActiveFrame {
    node: NodeId,
    kind: ActiveKind,
}

/// Per-thread profiler state: a flat node arena plus an active-span stack.
#[derive(Debug)]
struct ThreadState {
    /// Active-span stack (LIFO).  The top frame is the current parent.
    stack: Vec<ActiveFrame>,
    /// Flat arena — nodes are addressed by [`NodeId`] (index).
    nodes: Vec<Node>,
    /// Top-level root node ids (spans entered with no parent).
    roots: Vec<NodeId>,
    /// Last-seen root, for fast consecutive-root deduplication.
    roots_last_child: Option<NodeId>,
}

impl ThreadState {
    /// Create an empty thread state with pre-allocated capacity.
    fn new() -> Self {
        Self {
            stack: Vec::with_capacity(64),
            nodes: Vec::with_capacity(256),
            roots: Vec::with_capacity(16),
            roots_last_child: None,
        }
    }

    /// Append a fresh zero-initialised node to the arena and return its id.
    #[inline(always)]
    fn alloc_node(&mut self, id: &'static SpanId, tag: Option<Tag>) -> NodeId {
        let idx = self.nodes.len();
        self.nodes.push(Node {
            id,
            tag,
            wall_time_ns: 0,
            cpu_time_ns: 0,
            entered_count: 0,
            sampled_count: 0,
            children: Vec::new(),
            last_child: None,
            records: Vec::with_capacity(4),
            counters: Vec::with_capacity(4),
        });
        idx as NodeId
    }

    /// Shared reference to a node by arena index.
    #[inline(always)]
    fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Mutable reference to a node by arena index.
    #[inline(always)]
    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id as usize]
    }

    /// Look up or allocate a root-level node for the given `(SpanId, Tag)`.
    #[inline]
    fn find_or_create_root(&mut self, id: &'static SpanId, tag: Option<Tag>) -> NodeId {
        if let Some(last) = self.roots_last_child {
            if self.node(last).key_eq(id, tag) {
                return last;
            }
        }

        for &child in &self.roots {
            if self.node(child).key_eq(id, tag) {
                self.roots_last_child = Some(child);
                return child;
            }
        }

        let child = self.alloc_node(id, tag);
        self.roots.push(child);
        self.roots_last_child = Some(child);
        child
    }

    /// Look up or allocate a child node under `parent` for the given key.
    #[inline]
    fn find_or_create_child(
        &mut self,
        parent: NodeId,
        id: &'static SpanId,
        tag: Option<Tag>,
    ) -> NodeId {
        if let Some(last) = self.node(parent).last_child {
            if self.node(last).key_eq(id, tag) {
                return last;
            }
        }

        let children: &[NodeId] = &self.node(parent).children;
        for &child in children {
            if self.node(child).key_eq(id, tag) {
                self.node_mut(parent).last_child = Some(child);
                return child;
            }
        }

        let child = self.alloc_node(id, tag);
        let p = self.node_mut(parent);
        p.children.push(child);
        p.last_child = Some(child);
        child
    }

    /// The node id of the currently active (innermost) span, if any.
    #[inline(always)]
    fn current_parent(&self) -> Option<NodeId> {
        self.stack.last().map(|f| f.node)
    }

    /// Resolve (find-or-create) the node for a span, either as a root or
    /// as a child of the current parent.
    #[inline(always)]
    fn resolve_node(&mut self, id: &'static SpanId, tag: Option<Tag>) -> NodeId {
        match self.current_parent() {
            None => self.find_or_create_root(id, tag),
            Some(parent) => self.find_or_create_child(parent, id, tag),
        }
    }

    /// Convert the arena into a tree of [`ProfileStats`], consuming nodes in place.
    fn materialize_node(nodes: &mut Vec<Option<Node>>, node_id: NodeId) -> ProfileStats {
        let node = nodes[node_id as usize]
            .take()
            .expect("node already materialized or missing");

        let mut children = Vec::with_capacity(node.children.len());
        for &child_id in &node.children {
            children.push(Self::materialize_node(nodes, child_id));
        }

        ProfileStats {
            id: node.id,
            tag: node.tag,
            wall_time_ns: node.wall_time_ns,
            cpu_time_ns: node.cpu_time_ns,
            children,
            entered_count: node.entered_count,
            sampled_count: node.sampled_count,
            records: node.records,
            counters: node.counters,
        }
    }

    /// Drain the arena into a `Vec<ProfileStats>` tree and reset state.
    fn take_results_and_reset(&mut self) -> Vec<ProfileStats> {
        debug_assert!(
            self.stack.is_empty(),
            "take_results called while spans are still active"
        );

        let nodes = std::mem::take(&mut self.nodes);
        let roots = std::mem::take(&mut self.roots);

        let mut nodes_opt: Vec<Option<Node>> = nodes.into_iter().map(Some).collect();

        let mut out = Vec::with_capacity(roots.len());
        for root in roots {
            out.push(Self::materialize_node(&mut nodes_opt, root));
        }

        self.stack.clear();
        self.roots_last_child = None;

        out
    }

    /// Discard all accumulated nodes and reset the arena.
    fn clear(&mut self) {
        self.stack.clear();
        self.nodes.clear();
        self.roots.clear();
        self.roots_last_child = None;
    }
}

thread_local! {
    /// Per-thread profiler state (arena + stack).  Accessed via `RefCell`
    /// so that `begin_span` / `end_span` can borrow mutably.
    static STATE: RefCell<ThreadState> = RefCell::new(ThreadState::new());
}

thread_local! {
    /// Suppression nesting depth.  Kept separate from [`STATE`] so that
    /// `is_suppressed()` can be checked without borrowing the `RefCell`.
    static SUPPRESS_DEPTH: Cell<u32> = Cell::new(0);
}

/// A static, callsite-unique identifier for a profiling span.
///
/// A `SpanId` is typically created once per callsite (via a `OnceLock` inside
/// the [`span!`] macro or [the `#[profile]` attribute](profile)) and then
/// reused on every subsequent invocation.
///
/// Two `SpanId`s are considered equal when all four fields match.  As an
/// optimization, pointer equality is tried first — this is correct because
/// callsite-generated `SpanId`s use `&'static str` literals that are
/// guaranteed pointer-unique per callsite.
#[derive(Debug, Copy, Clone, Eq, Hash)]
pub struct SpanId {
    /// Human-readable span name (e.g., `"execute_tx"`).
    pub name: &'static str,
    /// Optional context qualifier, typically the module path.
    pub context: Option<&'static str>,
    /// Source file where the span was defined.
    pub file: &'static str,
    /// Source line where the span was defined.
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
    /// Create a `SpanId` from static strings and a caller [`Location`].
    #[inline(always)]
    fn new_from_loc(name: &'static str, loc: &'static Location) -> Self {
        Self {
            name,
            context: None,
            file: loc.file(),
            line: loc.line(),
        }
    }

    /// Attach a context qualifier (typically the module path).
    #[inline(always)]
    pub fn with_context(mut self, context: &'static str) -> Self {
        self.context = Some(context);
        self
    }
}

/// Collected metrics for one node in the profiling tree.
///
/// Each node aggregates timing from every sampled entry of the same
/// `(SpanId, Tag)` pair under the same parent path.  The tree mirrors
/// the dynamic call structure observed at runtime.
#[derive(Debug, Clone)]
pub struct ProfileStats {
    /// The callsite identity.
    pub id: &'static SpanId,
    /// Optional discriminator (see [`Tag`]).
    pub tag: Option<Tag>,
    /// Cumulative wall-clock time (nanoseconds) across all sampled entries.
    pub wall_time_ns: u64,
    /// Cumulative CPU time (nanoseconds) across all sampled entries.
    pub cpu_time_ns: u64,
    /// Child nodes in the call tree.
    pub children: Vec<ProfileStats>,
    /// Total number of times this span was entered (sampled **and** count-only).
    pub entered_count: usize,
    /// Number of entries that were fully timed (a subset of `entered_count`).
    pub sampled_count: usize,
    /// Per-occurrence key/value records (see [`record!`]).
    pub records: Vec<Record>,
    /// Aggregated counters (see [`counter_add!`]).
    pub counters: Vec<Counter>,
}

impl ProfileStats {
    /// Span name (e.g., `"execute_tx"`).
    pub fn name(&self) -> &'static str {
        self.id.name
    }

    /// Module-path context, if set.
    pub fn context(&self) -> Option<&'static str> {
        self.id.context
    }

    /// Source file where the span was defined.
    pub fn source_file(&self) -> &'static str {
        self.id.file
    }

    /// Source line where the span was defined.
    pub fn source_line(&self) -> u32 {
        self.id.line
    }

    /// The optional [`Tag`] discriminator.
    pub fn tag(&self) -> Option<&Tag> {
        self.tag.as_ref()
    }

    /// Total times this span was entered (sampled + count-only).
    pub fn count(&self) -> usize {
        self.entered_count
    }

    /// Estimated time the thread was **not** running on a CPU core
    /// (wall time minus CPU time).  See [`platform`](crate::platform)
    /// module docs for per-platform resolution caveats.
    pub fn wait_time(&self) -> Duration {
        Duration::from_nanos(self.wall_time_ns.saturating_sub(self.cpu_time_ns))
    }

    /// Cumulative wall-clock time as a [`Duration`].
    pub fn wall_time(&self) -> Duration {
        Duration::from_nanos(self.wall_time_ns)
    }

    /// Cumulative CPU time as a [`Duration`].
    pub fn cpu_time(&self) -> Duration {
        Duration::from_nanos(self.cpu_time_ns)
    }

    /// Wait time in nanoseconds (convenience for `wall - cpu`).
    pub fn wait_time_ns(&self) -> u64 {
        self.wall_time_ns.saturating_sub(self.cpu_time_ns)
    }

    /// Wall-clock time truncated to whole microseconds.
    pub fn wall_time_micros(&self) -> u64 {
        self.wall_time_ns / 1_000
    }

    /// CPU time truncated to whole microseconds.
    pub fn cpu_time_micros(&self) -> u64 {
        self.cpu_time_ns / 1_000
    }

    /// Print the tree to stdout using the built-in [`PrettyPrinter`](crate::print::PrettyPrinter).
    pub fn print_tree(&self) {
        crate::print::print_tree(self, &crate::print::PrettyPrinter);
    }

    /// Print the tree to stdout using a custom [`TreeFormatter`](crate::print::TreeFormatter).
    pub fn print_with<F: crate::print::TreeFormatter>(&self, formatter: &F) {
        crate::print::print_tree(self, formatter);
    }
}

// ── public API ─────────────────────────────────────────────────────────

/// Static entry-point for all profiler operations.
///
/// `Profiler` is a zero-sized struct with only associated functions.
/// Most users should prefer the [`span!`], [`measure!`], and
/// [`#[profile]`](profile) macros, which handle `SpanId` caching and
/// guard lifetime automatically.
pub struct Profiler;

impl Profiler {
    /// Create a new [`SpanId`] anchored at the caller's source location.
    ///
    /// Typically called once per callsite inside a `OnceLock`; the macros
    /// handle this automatically.
    #[inline(always)]
    #[track_caller]
    pub fn new_span_id(name: &'static str) -> SpanId {
        let loc = Location::caller();
        SpanId::new_from_loc(name, loc)
    }

    /// Begin a **timed** span.  Wall and CPU clocks are read on entry;
    /// elapsed time is accumulated when the returned guard is dropped.
    #[inline(always)]
    pub fn begin_span(id: &'static SpanId, tag: Option<Tag>) -> ProfileGuard {
        let start_wall = Instant::now();
        let start_cpu_ns = crate::platform::thread_cpu_nanos();

        STATE.with(|cell| {
            let mut st = cell.borrow_mut();
            let node = st.resolve_node(id, tag);
            st.stack.push(ActiveFrame {
                node,
                kind: ActiveKind::Timed {
                    start_wall,
                    start_cpu_ns,
                },
            });
        });

        ProfileGuard {
            kind: GuardKind::Span,
        }
    }

    /// Begin a **count-only** span — preserves hierarchy and increments
    /// `entered_count`, but does **not** read clocks.
    #[inline(always)]
    pub fn begin_span_count_only(id: &'static SpanId, tag: Option<Tag>) -> ProfileGuard {
        STATE.with(|cell| {
            let mut st = cell.borrow_mut();
            let node = st.resolve_node(id, tag);
            st.stack.push(ActiveFrame {
                node,
                kind: ActiveKind::CountOnly,
            });
        });

        ProfileGuard {
            kind: GuardKind::Span,
        }
    }

    /// Enter a **suppression** region.  While suppressed, nested
    /// `span!`/`measure!` calls return `None` (no-op), preventing
    /// children from attaching to the wrong parent.
    #[inline(always)]
    pub fn begin_suppression() -> ProfileGuard {
        SUPPRESS_DEPTH.with(|d| d.set(d.get().wrapping_add(1)));
        ProfileGuard {
            kind: GuardKind::Suppression,
        }
    }

    #[inline]
    #[doc(hidden)]
    pub fn end_span() {
        STATE.with(|cell| {
            let mut st = cell.borrow_mut();
            let Some(frame) = st.stack.pop() else {
                return;
            };

            let node = st.node_mut(frame.node);

            match frame.kind {
                ActiveKind::Timed {
                    start_wall,
                    start_cpu_ns,
                } => {
                    let end_wall = Instant::now();
                    let end_cpu_ns = crate::platform::thread_cpu_nanos();

                    let wall_ns = end_wall.duration_since(start_wall).as_nanos() as u64;
                    let cpu_ns = end_cpu_ns.saturating_sub(start_cpu_ns);

                    node.wall_time_ns += wall_ns;
                    node.cpu_time_ns += cpu_ns;
                    node.entered_count += 1;
                    node.sampled_count += 1;
                }
                ActiveKind::CountOnly => {
                    node.entered_count += 1;
                }
            }
        });
    }

    #[doc(hidden)]
    #[inline(always)]
    pub fn is_suppressed() -> bool {
        SUPPRESS_DEPTH.with(|d| d.get() != 0)
    }

    #[inline(always)]
    #[doc(hidden)]
    pub fn end_suppression() {
        SUPPRESS_DEPTH.with(|d| d.set(d.get().wrapping_sub(1)));
    }

    /// Enable record/counter attachment (**process-global** default: enabled).
    #[inline(always)]
    pub fn enable_record() {
        RECORD_ENABLED.store(true, Ordering::Relaxed);
    }

    /// Disable record/counter attachment process-wide.  Spans and timing
    /// are unaffected — only [`record!`] and [`counter_add!`] become no-ops.
    #[inline(always)]
    pub fn disable_record() {
        RECORD_ENABLED.store(false, Ordering::Relaxed);
    }

    /// Returns `true` if record/counter attachment is currently enabled.
    #[inline(always)]
    pub fn is_record_enabled() -> bool {
        RECORD_ENABLED.load(Ordering::Relaxed)
    }

    /// Attach a key/value [`Record`] to the innermost active span on this
    /// thread.  No-op if recording is disabled, suppressed, or no span
    /// is active.
    #[inline]
    pub fn record(key: &'static str, value: RecordValue) {
        if !RECORD_ENABLED.load(Ordering::Relaxed) {
            return;
        }
        if Self::is_suppressed() {
            return;
        }

        STATE.with(|cell| {
            let mut st = cell.borrow_mut();
            let (node_id, is_count_only) = match st.stack.last() {
                Some(frame) => (frame.node, matches!(frame.kind, ActiveKind::CountOnly)),
                None => return,
            };

            // Skip count-only spans (no timing) to avoid noisy data.
            if is_count_only {
                return;
            }

            let node = st.node_mut(node_id);
            node.records.push(Record { key, value });
        });
    }

    /// Add `delta` to the named [`Counter`] on the innermost active span.
    /// Counters with the same key are summed (saturating).
    #[inline]
    pub fn counter_add(key: &'static str, delta: u64) {
        if !RECORD_ENABLED.load(Ordering::Relaxed) {
            return;
        }
        if Self::is_suppressed() {
            return;
        }

        STATE.with(|cell| {
            let mut st = cell.borrow_mut();
            let (node_id, is_count_only) = match st.stack.last() {
                Some(frame) => (frame.node, matches!(frame.kind, ActiveKind::CountOnly)),
                None => return,
            };

            if is_count_only {
                return;
            }

            let node = st.node_mut(node_id);
            if let Some(counter) = node.counters.iter_mut().find(|c| c.key == key) {
                counter.value = counter.value.saturating_add(delta);
            } else {
                node.counters.push(Counter { key, value: delta });
            }
        });
    }

    /// Drain the calling thread's profile tree and return it as a
    /// `Vec<ProfileStats>` (one entry per root span).  The thread-local
    /// state is reset afterward.
    ///
    /// # Panics (debug)
    ///
    /// Debug-asserts that no spans are currently active on this thread.
    #[inline]
    pub fn take_results() -> Vec<ProfileStats> {
        STATE.with(|cell| cell.borrow_mut().take_results_and_reset())
    }

    /// Discard all accumulated data on the calling thread without
    /// returning it.  Suppression depth is **not** affected (it is
    /// scoped by guards).
    #[inline]
    pub fn clear() {
        STATE.with(|cell| cell.borrow_mut().clear())
        // NOTE: do not touch SUPPRESS_DEPTH here; suppression is scoped by guards.
    }
}

/// Discriminates the two kinds of RAII guard so that `Drop` calls the
/// correct cleanup path.
enum GuardKind {
    /// Timed or count-only — calls `end_span()` on drop.
    Span,
    /// Suppression region — calls `end_suppression()` on drop.
    Suppression,
}

/// RAII guard that ends a span (or suppression region) when dropped.
///
/// Created by [`Profiler::begin_span`], [`Profiler::begin_span_count_only`],
/// or [`Profiler::begin_suppression`] (and transitively by the [`span!`] and
/// [`measure!`] macros).
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
