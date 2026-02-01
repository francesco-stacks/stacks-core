export function toMs(value) {
  if (value === null || value === undefined) return null;
  return value / 1000;
}

export function toAvgMs(totalUs, row) {
  if (totalUs === null || totalUs === undefined) return null;
  const calls = row?.call_count ?? 0;
  if (!Number.isFinite(calls) || calls <= 0) return null;
  return (totalUs / calls) / 1000;
}

export function getWallUs(row) {
  return row.est_wall_us ?? row.wall_time_us ?? null;
}

export function getSelfWallUs(row) {
  return row.est_self_wall_us ?? row.self_wall_time_us ?? null;
}

export function getSelfCpuUs(row) {
  return row.est_self_cpu_us ?? row.self_cpu_time_us ?? null;
}

export function getSelfWaitUs(row) {
  const wall = getSelfWallUs(row);
  const cpu = getSelfCpuUs(row);
  if (wall == null || cpu == null) return null;
  return Math.max(0, wall - cpu);
}

export function flattenTree(nodes) {
  const rows = [];
  const walk = (node) => {
    rows.push(node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return rows;
}

export function buildTreeIndex(nodes) {
  const byId = new Map();
  nodes.forEach((node) => {
    byId.set(node.id, { ...node, data: [] });
  });
  const roots = [];
  byId.forEach((node) => {
    if (node.parent_id && byId.has(node.parent_id)) {
      byId.get(node.parent_id).data.push(node);
    } else {
      roots.push(node);
    }
  });
  const sortByPath = (a, b) => (a.sort_path || "").localeCompare(b.sort_path || "");
  roots.sort(sortByPath);
  byId.forEach((node) => node.data.sort(sortByPath));
  return { roots, byId };
}

export function pruneTree(nodes, minWallUs) {
  if (!minWallUs) return nodes.map((node) => ({ ...node, data: pruneTree(node.data || [], minWallUs) }));
  return nodes
    .map((node) => {
      const children = pruneTree(node.data || [], minWallUs);
      const wall = getWallUs(node);
      const keep = (wall != null && wall >= minWallUs) || children.length > 0;
      return keep ? { ...node, data: children } : null;
    })
    .filter(Boolean);
}

export function indexTree(nodes) {
  const byId = new Map();
  const walk = (node) => {
    byId.set(node.id, node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return byId;
}

export function applyFocus(roots, byId, focusId) {
  if (focusId == null || focusId === "") return { roots, byId, breadcrumb: [] };
  const focusNode = byId.get(focusId);
  if (!focusNode) return { roots, byId, breadcrumb: [] };
  const breadcrumb = [];
  let current = focusNode;
  while (current) {
    breadcrumb.unshift(current);
    if (!current.parent_id) break;
    current = byId.get(current.parent_id);
  }
  return { roots: [{ ...focusNode, data: focusNode.data || [] }], byId, breadcrumb };
}

export function applyHotPath(nodes, mode) {
  if (mode === "off") return nodes.map((node) => ({ ...node, data: applyHotPath(node.data || [], mode) }));
  const metric = (node) => (mode === "self" ? getSelfWallUs(node) ?? 0 : getWallUs(node) ?? 0);
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

export function applyChainCompression(nodes, { enabled, expandedChains, significantSelfUs }) {
  const isSignificant = (node) =>
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

  const aggregateNumeric = (items) => {
    if (items.length === 0) return {};
    const aggregate = {};
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

  const compressNode = (node) => {
    if (!enabled || expandedChains.has(node.id) || isSignificant(node)) {
      return { ...node, data: (node.data || []).map(compressNode) };
    }
    const chain = [node];
    let cursor = node;
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

export function applyOpenState(nodes, openNodes, forceOpenAll) {
  const walk = (node, depth) => {
    const open = forceOpenAll ? true : depth === 0 || openNodes.has(node.id);
    return {
      ...node,
      open,
      data: (node.data || []).map((child) => walk(child, depth + 1)),
    };
  };
  return nodes.map((node) => walk(node, 0));
}

export function collectSubtreeIds(node, ids) {
  ids.add(node.id);
  (node.data || []).forEach((child) => collectSubtreeIds(child, ids));
}

export function computeDefaultOpenSet(nodes, defaultAutoExpand) {
  const openIds = new Set();
  const walk = (node, depth) => {
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
