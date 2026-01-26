use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::{ProfileStats, Record, SpanId, Tag};

type NodeId = u32;

static RECORD_ENABLED: AtomicBool = AtomicBool::new(true);

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
}

impl Node {
    #[inline(always)]
    fn key_eq(&self, id: &'static SpanId, tag: Option<Tag>) -> bool {
        // Fast path: callsite SpanIds are typically pointer-unique.
        std::ptr::eq(self.id, id) && self.tag == tag
    }
}

#[derive(Debug)]
enum ActiveKind {
    Timed {
        start_wall: Instant,
        start_cpu_ns: u64,
    },
    CountOnly,
}

#[derive(Debug)]
struct ActiveFrame {
    node: NodeId,
    kind: ActiveKind,
}

#[derive(Debug)]
struct ThreadState {
    stack: Vec<ActiveFrame>,
    nodes: Vec<Node>,
    roots: Vec<NodeId>,
    roots_last_child: Option<NodeId>,
}

impl ThreadState {
    fn new() -> Self {
        Self {
            stack: Vec::with_capacity(64),
            nodes: Vec::with_capacity(256),
            roots: Vec::with_capacity(16),
            roots_last_child: None,
        }
    }

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
        });
        idx as NodeId
    }

    #[inline(always)]
    fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    #[inline(always)]
    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id as usize]
    }

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

    #[inline(always)]
    fn current_parent(&self) -> Option<NodeId> {
        self.stack.last().map(|f| f.node)
    }

    #[inline(always)]
    fn resolve_node(&mut self, id: &'static SpanId, tag: Option<Tag>) -> NodeId {
        match self.current_parent() {
            None => self.find_or_create_root(id, tag),
            Some(parent) => self.find_or_create_child(parent, id, tag),
        }
    }

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
            records: node.records, // moved
        }
    }

    fn take_results_and_reset(&mut self) -> Vec<ProfileStats> {
        debug_assert!(
            self.stack.is_empty(),
            "take_results called while spans are still active"
        );

        // Move nodes and roots out, so we can consume nodes without cloning.
        let nodes = std::mem::take(&mut self.nodes);
        let roots = std::mem::take(&mut self.roots);

        // Convert into Option<Node> so we can `take()` by index.
        let mut nodes_opt: Vec<Option<Node>> = nodes.into_iter().map(Some).collect();

        let mut out = Vec::with_capacity(roots.len());
        for root in roots {
            out.push(Self::materialize_node(&mut nodes_opt, root));
        }

        self.stack.clear();
        self.roots_last_child = None;

        out
    }

    fn clear(&mut self) {
        self.stack.clear();
        self.nodes.clear();
        self.roots.clear();
        self.roots_last_child = None;
    }
}

thread_local! {
    static STATE: RefCell<ThreadState> = RefCell::new(ThreadState::new());
}

// Suppression is separate from STATE so `span!` can check cheaply without RefCell borrows.
thread_local! {
    static SUPPRESS_DEPTH: Cell<u32> = Cell::new(0);
}

#[inline(always)]
pub fn is_suppressed() -> bool {
    SUPPRESS_DEPTH.with(|d| d.get() != 0)
}

#[inline(always)]
pub fn begin_suppression() {
    SUPPRESS_DEPTH.with(|d| d.set(d.get().wrapping_add(1)));
}

#[inline(always)]
pub fn end_suppression() {
    SUPPRESS_DEPTH.with(|d| d.set(d.get().wrapping_sub(1)));
}

#[inline(always)]
pub fn begin_span_timed(id: &'static SpanId, tag: Option<Tag>) {
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
}

#[inline(always)]
pub fn begin_span_count_only(id: &'static SpanId, tag: Option<Tag>) {
    STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        let node = st.resolve_node(id, tag);
        st.stack.push(ActiveFrame {
            node,
            kind: ActiveKind::CountOnly,
        });
    });
}

#[inline]
pub fn end_span() {
    // Pop first, then decide whether we need clocks.
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

#[inline(always)]
pub fn enable_record() {
    RECORD_ENABLED.store(true, Ordering::Relaxed);
}

#[inline(always)]
pub fn disable_record() {
    RECORD_ENABLED.store(false, Ordering::Relaxed);
}

#[inline(always)]
pub fn is_record_enabled() -> bool {
    RECORD_ENABLED.load(Ordering::Relaxed)
}

#[inline]
pub fn record_kv(key: &'static str, value: crate::RecordValue) {
    // Return early if recording is disabled.
    if !is_record_enabled() {
        return;
    }

    // Ignore if suppressed or no active span.
    if is_suppressed() {
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

        // // Bounded storage: keep last N records.
        // const MAX_RECORDS: usize = 8;
        // if node.records.len() == MAX_RECORDS {
        //     // drop oldest
        //     node.records.remove(0);
        // }
        node.records.push(crate::Record { key, value });
    });
}

#[inline]
pub fn take_results() -> Vec<ProfileStats> {
    STATE.with(|cell| cell.borrow_mut().take_results_and_reset())
}

#[inline]
pub fn clear() {
    STATE.with(|cell| cell.borrow_mut().clear())
    // NOTE: do not touch SUPPRESS_DEPTH here; suppression is scoped by guards.
}
