use std::cell::{Cell, RefCell};
use std::time::Instant;

use crate::{ProfileStats, SpanId, Tag};

type NodeId = u32;

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

    fn materialize_node(&self, node_id: NodeId) -> ProfileStats {
        let node = self.node(node_id);

        let mut children = Vec::with_capacity(node.children.len());
        for &child_id in &node.children {
            children.push(self.materialize_node(child_id));
        }

        ProfileStats {
            id: node.id,
            tag: node.tag,
            wall_time_ns: node.wall_time_ns,
            cpu_time_ns: node.cpu_time_ns,
            children,
            entered_count: node.entered_count,
            sampled_count: node.sampled_count,
        }
    }

    fn take_results_and_reset(&mut self) -> Vec<ProfileStats> {
        debug_assert!(
            self.stack.is_empty(),
            "take_results called while spans are still active"
        );

        let mut out = Vec::with_capacity(self.roots.len());
        for &root in &self.roots {
            out.push(self.materialize_node(root));
        }

        self.stack.clear();
        self.nodes.clear();
        self.roots.clear();
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

#[inline]
pub fn take_results() -> Vec<ProfileStats> {
    STATE.with(|cell| cell.borrow_mut().take_results_and_reset())
}

#[inline]
pub fn clear() {
    STATE.with(|cell| cell.borrow_mut().clear())
    // NOTE: do not touch SUPPRESS_DEPTH here; suppression is scoped by guards.
}
