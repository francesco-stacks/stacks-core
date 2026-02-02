export type TreeNode = Record<string, any> & {
  id: string | number;
  parent_id?: string | number | null;
  data?: TreeNode[];
  sort_path?: string | null;
};

type DefaultAutoExpand = {
  depth: number;
  selfMs: number;
  wallMs: number;
  topKChildren: number;
};

type ChainCompressionOptions = {
  enabled: boolean;
  expandedChains: Set<string | number>;
  significantSelfUs?: number | null;
};

export function toMs(value: number | null | undefined): number | null {
  if (value === null || value === undefined) return null;
  return value / 1000;
}

export function toAvgMs(totalUs: number | null | undefined, row: { call_count?: number } | null | undefined): number | null {
  if (totalUs === null || totalUs === undefined) return null;
  const calls = row?.call_count ?? 0;
  if (!Number.isFinite(calls) || calls <= 0) return null;
  return (totalUs / calls) / 1000;
}

export function getWallUs(row: Record<string, any>): number | null {
  return row.est_wall_us ?? row.wall_time_us ?? null;
}

export function getSelfWallUs(row: Record<string, any>): number | null {
  return row.est_self_wall_us ?? row.self_wall_time_us ?? null;
}

export function getSelfCpuUs(row: Record<string, any>): number | null {
  return row.est_self_cpu_us ?? row.self_cpu_time_us ?? null;
}

export function getSelfWaitUs(row: Record<string, any>): number | null {
  const wall = getSelfWallUs(row);
  const cpu = getSelfCpuUs(row);
  if (wall == null || cpu == null) return null;
  return Math.max(0, wall - cpu);
}

export function flattenTree(nodes: TreeNode[]): TreeNode[] {
  const rows: TreeNode[] = [];
  const walk = (node: TreeNode) => {
    rows.push(node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return rows;
}

export function buildTreeIndex(nodes: TreeNode[]) {
  const byId = new Map<string | number, TreeNode>();
  nodes.forEach((node) => {
    byId.set(node.id, { ...node, data: [] });
  });
  const roots: TreeNode[] = [];
  byId.forEach((node) => {
    if (node.parent_id && byId.has(node.parent_id)) {
      byId.get(node.parent_id)!.data!.push(node);
    } else {
      roots.push(node);
    }
  });
  const sortByPath = (a: TreeNode, b: TreeNode) => (a.sort_path || "").localeCompare(b.sort_path || "");
  roots.sort(sortByPath);
  byId.forEach((node) => node.data!.sort(sortByPath));
  return { roots, byId };
}

export function pruneTree(nodes: TreeNode[], minWallUs?: number | null): TreeNode[] {
  if (!minWallUs) return nodes.map((node) => ({ ...node, data: pruneTree(node.data || [], minWallUs) }));
  return nodes
    .map((node) => {
      const children = pruneTree(node.data || [], minWallUs);
      const wall = getWallUs(node);
      const keep = (wall != null && wall >= minWallUs) || children.length > 0;
      return keep ? { ...node, data: children } : null;
    })
    .filter(Boolean) as TreeNode[];
}

export function indexTree(nodes: TreeNode[]) {
  const byId = new Map<string | number, TreeNode>();
  const walk = (node: TreeNode) => {
    byId.set(node.id, node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return byId;
}

export function applyFocus(roots: TreeNode[], byId: Map<string | number, TreeNode>, focusId?: string | number | null) {
  if (focusId == null || focusId === "") return { roots, byId, breadcrumb: [] as TreeNode[] };
  const focusNode = byId.get(focusId);
  if (!focusNode) return { roots, byId, breadcrumb: [] as TreeNode[] };
  const breadcrumb: TreeNode[] = [];
  let current: TreeNode | undefined = focusNode;
  while (current) {
    breadcrumb.unshift(current);
    if (!current.parent_id) break;
    current = byId.get(current.parent_id) || undefined;
  }
  return { roots: [{ ...focusNode, data: focusNode.data || [] }], byId, breadcrumb };
}

export function applyHotPath(nodes: TreeNode[], mode: "off" | "self" | "total"): TreeNode[] {
  if (mode === "off") return nodes.map((node) => ({ ...node, data: applyHotPath(node.data || [], mode) }));
  const metric = (node: TreeNode) => (mode === "self" ? getSelfWallUs(node) ?? 0 : getWallUs(node) ?? 0);
  return nodes.map((node) => {
    if (!node.data || node.data.length === 0) return { ...node, data: [] };
    const sorted = [...node.data].sort((a, b) => metric(b) - metric(a));
    const best = sorted[0];
    const hidden = node.data.length - 1;
    return {
      ...node,
      hidden_siblings: hidden,
      data: best ? applyHotPath([best], mode) : [],
    };
  });
}

export function applyChainCompression(nodes: TreeNode[], { enabled, expandedChains, significantSelfUs }: ChainCompressionOptions) {
  const isSignificant = (node: TreeNode) =>
    significantSelfUs != null && (getSelfWallUs(node) ?? 0) >= significantSelfUs;

  const maxKeys = new Set(["wall_time_us", "est_wall_us", "cpu_time_us", "est_cpu_us"]);
  const skipKeys = new Set([
    "id",
    "parent_id",
    "depth",
    "open",
    "data",
    "sort_path",
    "span_name",
    "tag",
    "chain_count",
    "chain_label",
    "chain_full_label",
    "chain_tags",
    "chain_segments",
    "flame_percent",
  ]);

  const aggregateNumeric = (items: TreeNode[]) => {
    if (items.length === 0) return {} as Record<string, number>;
    const aggregate: Record<string, number> = {};
    items.forEach((item) => {
      Object.entries(item).forEach(([key, value]) => {
        if (skipKeys.has(key)) return;
        if (typeof value !== "number") return;
        if (maxKeys.has(key)) {
          aggregate[key] = Math.max(aggregate[key] ?? 0, value);
        } else {
          aggregate[key] = (aggregate[key] ?? 0) + value;
        }
      });
    });
    return aggregate;
  };

  const compressNode = (node: TreeNode): TreeNode => {
    if (!enabled || expandedChains.has(node.id) || isSignificant(node)) {
      return { ...node, data: (node.data || []).map(compressNode) };
    }
    const chain: TreeNode[] = [node];
    let cursor: TreeNode = node;
    while (
      cursor.data &&
      cursor.data.length === 1 &&
      !expandedChains.has(cursor.data[0].id) &&
      !isSignificant(cursor.data[0])
    ) {
      cursor = cursor.data[0];
      chain.push(cursor);
    }
    const tailChildren = (cursor.data || []).map(compressNode);
    if (chain.length <= 1) return { ...node, data: tailChildren };
    const labelSegments = chain.map((item) => item.span_name ?? "-");
    const chainLabel = labelSegments.join(" › ");
    const chainTags = Array.from(
      new Set(chain.map((item) => item.tag).filter((tag) => tag && String(tag).trim().length > 0))
    );
    const chainSegments = chain.map((item) => ({
      name: item.span_name ?? "-",
      tag: item.tag ?? null,
    }));
    const aggregated = aggregateNumeric(chain);
    return {
      ...node,
      ...aggregated,
      data: tailChildren,
      chain_count: chain.length - 1,
      chain_label: chainLabel,
      chain_full_label: chainLabel,
      chain_tags: chainTags,
      chain_segments: chainSegments,
    };
  };

  return nodes.map(compressNode);
}

export function applyOpenState(nodes: TreeNode[], openNodes: Set<string | number>, forceOpenAll: boolean) {
  const walk = (node: TreeNode, depth: number): TreeNode => {
    const open = forceOpenAll ? true : depth === 0 || openNodes.has(node.id);
    return {
      ...node,
      open,
      data: (node.data || []).map((child) => walk(child, depth + 1)),
    };
  };
  return nodes.map((node) => walk(node, 0));
}

export function collectSubtreeIds(node: TreeNode, ids: Set<string | number>) {
  ids.add(node.id);
  (node.data || []).forEach((child) => collectSubtreeIds(child, ids));
}

export function computeDefaultOpenSet(nodes: TreeNode[], defaultAutoExpand: DefaultAutoExpand) {
  const openIds = new Set<string | number>();
  const walk = (node: TreeNode, depth: number) => {
    const wallMs = (getWallUs(node) ?? 0) / 1000;
    const selfMs = (getSelfWallUs(node) ?? 0) / 1000;
    if (
      depth < defaultAutoExpand.depth ||
      selfMs >= defaultAutoExpand.selfMs ||
      wallMs >= defaultAutoExpand.wallMs
    ) {
      openIds.add(node.id);
    }
    if (node.data && node.data.length > 0) {
      const sorted = [...node.data].sort((a, b) => (getWallUs(b) ?? 0) - (getWallUs(a) ?? 0));
      sorted.slice(0, defaultAutoExpand.topKChildren).forEach((child) => openIds.add(child.id));
      node.data.forEach((child) => walk(child, depth + 1));
    }
  };
  nodes.forEach((node) => walk(node, 0));
  return openIds;
}
