import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Grid } from "@svar-ui/react-grid";
import { WillowDark } from "@svar-ui/react-core";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Sun, Moon, RotateCcw, Settings, Loader2, Columns3, X } from "lucide-react";

const DEFAULT_AUTO_EXPAND = {
  depth: 2,
  selfMs: 5,
  wallMs: 20,
  topKChildren: 3,
};

const COLUMN_DEFS = [
  {
    key: "span",
    label: "Span",
    width: 360,
    flexgrow: 1,
    default: true,
    selectable: false,
    alwaysVisible: true,
    treetoggle: true,
  },
  {
    key: "calls",
    label: "Calls",
    width: 90,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.call_count ?? "-",
  },
  {
    key: "tag",
    label: "Tag",
    width: 180,
    default: false,
    getter: (row) => row.tag ?? "-",
  },
  {
    key: "wall_total",
    label: "Wall Total (ms)",
    headerLabel: "Total",
    group: "Wall Time (ms)",
    groupSpan: 2,
    groupStart: true,
    width: 140,
    default: true,
    heatKey: "wallTotalUs",
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_wall_us ?? row.wall_time_us),
  },
  {
    key: "wall_avg",
    label: "Wall Avg (ms)",
    headerLabel: "Avg.",
    group: "Wall Time (ms)",
    groupSpan: 2,
    groupStart: false,
    width: 140,
    default: true,
    format: { decimals: 3 },
    getter: (row) => toAvgMs(row.est_wall_us ?? row.wall_time_us, row),
  },
  {
    key: "wall_self",
    label: "Wall Self (ms)",
    width: 140,
    default: true,
    heatKey: "selfWallUs",
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_self_wall_us ?? row.self_wall_time_us),
  },
  {
    key: "busy_self",
    label: "Busy Self (ms)",
    width: 140,
    default: false,
    heatKey: "selfBusyUs",
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_self_cpu_us ?? row.self_cpu_time_us),
  },
  {
    key: "wait_self",
    label: "Wait Self (ms)",
    width: 140,
    default: false,
    heatKey: "selfWaitUs",
    getter: (row) => {
      const wall = row.est_self_wall_us ?? row.self_wall_time_us;
      const cpu = row.est_self_cpu_us ?? row.self_cpu_time_us;
      if (wall == null || cpu == null) return "-";
      return toMs(Math.max(0, wall - cpu));
    },
    format: { decimals: 3 },
  },
  {
    key: "busy_total",
    label: "Busy Total (ms)",
    headerLabel: "Total",
    group: "Busy Time (ms)",
    groupSpan: 2,
    groupStart: true,
    width: 140,
    default: false,
    heatKey: "busyTotalUs",
    format: { decimals: 3 },
    getter: (row) => toMs(row.est_cpu_us ?? row.cpu_time_us),
  },
  {
    key: "busy_avg",
    label: "Busy Avg (ms)",
    headerLabel: "Avg.",
    group: "Busy Time (ms)",
    groupSpan: 2,
    groupStart: false,
    width: 140,
    default: false,
    format: { decimals: 3 },
    getter: (row) => toAvgMs(row.est_cpu_us ?? row.cpu_time_us, row),
  },
  {
    key: "wait_total",
    label: "Wait Total (ms)",
    headerLabel: "Total",
    group: "Wait Time (ms)",
    groupSpan: 2,
    groupStart: true,
    width: 140,
    default: false,
    heatKey: "waitTotalUs",
    getter: (row) => {
      const wall = row.est_wall_us ?? row.wall_time_us;
      const cpu = row.est_cpu_us ?? row.cpu_time_us;
      if (wall == null || cpu == null) return "-";
      return toMs(Math.max(0, wall - cpu));
    },
    format: { decimals: 3 },
  },
  {
    key: "wait_avg",
    label: "Wait Avg (ms)",
    headerLabel: "Avg.",
    group: "Wait Time (ms)",
    groupSpan: 2,
    groupStart: false,
    width: 140,
    default: false,
    format: { decimals: 3 },
    getter: (row) => {
      const wall = row.est_wall_us ?? row.wall_time_us;
      const cpu = row.est_cpu_us ?? row.cpu_time_us;
      if (wall == null || cpu == null) return "-";
      return toAvgMs(Math.max(0, wall - cpu), row);
    },
  },
  {
    key: "samples",
    label: "Samples",
    width: 90,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.sample_count ?? "-",
  },
  {
    key: "kv_total",
    label: "KV Total",
    width: 100,
    default: true,
    format: { decimals: 0 },
    getter: (row) => row.kv_total ?? "-",
  },
  {
    key: "clarity_rw",
    label: "R/W Count",
    width: 120,
    default: true,
    getter: (row) => `${row.clarity_read_count ?? 0}/${row.clarity_write_count ?? 0}`,
  },
  {
    key: "clarity_len",
    label: "R/W Length",
    width: 120,
    default: false,
    getter: (row) => `${row.clarity_read_length ?? 0}/${row.clarity_write_length ?? 0}`,
  },
  {
    key: "clarity_runtime",
    label: "Runtime",
    width: 120,
    default: true,
    heatKey: "clarityRuntime",
    format: { decimals: 0 },
    getter: (row) => row.clarity_runtime ?? "-",
  },
  {
    key: "clarity_input_n",
    label: "Input n",
    width: 110,
    default: false,
    format: { decimals: 0 },
    getter: (row) => row.clarity_input_n ?? "-",
  },
  {
    key: "record_id",
    label: "Record ID",
    width: 120,
    default: false,
    selectable: false,
    alwaysHidden: true,
    format: { decimals: 0 },
    getter: (row) => row.id,
  },
];

const NUMBER_FORMATS = [
  { id: "space-dot", label: "1 000 000.00", group: " ", decimal: "." },
  { id: "dot-comma", label: "1.000.000,00", group: ".", decimal: "," },
  { id: "comma-dot", label: "1,000,000.00", group: ",", decimal: "." },
];

const DEFAULT_NUMBER_FORMAT_ID = "comma-dot";

const NUMERIC_COLUMN_KEYS = new Set([
  "calls",
  "samples",
  "kv_total",
  "wall_total",
  "wall_avg",
  "wall_self",
  "busy_self",
  "wait_self",
  "busy_total",
  "busy_avg",
  "wait_total",
  "wait_avg",
  "clarity_runtime",
  "clarity_input_n",
  "clarity_rw",
  "clarity_len",
  "record_id",
]);

const COUNT_DIM_KEYS = new Set([
  "clarity_rw",
  "clarity_len",
  "clarity_runtime",
  "clarity_input_n",
]);

const ALWAYS_VISIBLE_KEYS = new Set(
  COLUMN_DEFS.filter((c) => c.alwaysVisible).map((c) => c.key)
);
const ALWAYS_HIDDEN_KEYS = new Set(
  COLUMN_DEFS.filter((c) => c.alwaysHidden).map((c) => c.key)
);
const SELECTABLE_COLUMNS = COLUMN_DEFS.filter((c) => c.selectable !== false);

const DEFAULT_COLUMNS = COLUMN_DEFS.filter((c) => c.default)
  .map((c) => c.key)
  .filter((key) => !ALWAYS_HIDDEN_KEYS.has(key));

const sanitizeSelectedColumns = (values) => {
  const base = Array.isArray(values) ? values.filter((key) => !ALWAYS_HIDDEN_KEYS.has(key)) : [];
  ALWAYS_VISIBLE_KEYS.forEach((key) => {
    if (!base.includes(key)) base.push(key);
  });
  return base.length > 0 ? base : DEFAULT_COLUMNS;
};

function toMs(value) {
  if (value === null || value === undefined) return null;
  return value / 1000;
}

function toAvgMs(totalUs, row) {
  if (totalUs === null || totalUs === undefined) return null;
  const calls = row?.call_count ?? 0;
  if (!Number.isFinite(calls) || calls <= 0) return null;
  return (totalUs / calls) / 1000;
}

function getNumberFormat(id) {
  return NUMBER_FORMATS.find((format) => format.id === id) || NUMBER_FORMATS[2];
}

function formatNumberParts(value, { group, decimal, decimals }) {
  const sign = value < 0 ? "-" : "";
  const abs = Math.abs(value);
  const fixed = Number.isFinite(decimals) ? abs.toFixed(decimals) : String(abs);
  const [intPart, fracPart] = fixed.split(".");
  const grouped = group
    ? intPart.replace(/\B(?=(\d{3})+(?!\d))/g, group)
    : intPart;
  return {
    sign,
    int: grouped,
    frac: fracPart || "",
    decimal,
  };
}

function FormattedNumber({ value, format, decimals, className }) {
  if (value === null || value === undefined) {
    return <span className={`${className} numeric-zero`}>-</span>;
  }
  if (typeof value !== "number" || !Number.isFinite(value)) {
    if (typeof value === "string" && value.trim() === "0") {
      return <span className={`${className} numeric-zero`}>-</span>;
    }
    return <span className={className}>{String(value)}</span>;
  }
  if (value === 0) {
    return <span className={`${className} numeric-zero`}>-</span>;
  }
  const parts = formatNumberParts(value, {
    group: format.group,
    decimal: format.decimal,
    decimals,
  });
  return (
    <span className={className}>
      {parts.sign}
      {parts.int}
      {parts.frac ? (
        <span className="numeric-decimals">
          {parts.decimal}
          {parts.frac}
        </span>
      ) : null}
    </span>
  );
}

function getWallUs(row) {
  return row.est_wall_us ?? row.wall_time_us ?? null;
}

function getSelfWallUs(row) {
  return row.est_self_wall_us ?? row.self_wall_time_us ?? null;
}

function getSelfCpuUs(row) {
  return row.est_self_cpu_us ?? row.self_cpu_time_us ?? null;
}

function getSelfWaitUs(row) {
  const wall = getSelfWallUs(row);
  const cpu = getSelfCpuUs(row);
  if (wall == null || cpu == null) return null;
  return Math.max(0, wall - cpu);
}

function flattenTree(nodes) {
  const rows = [];
  const walk = (node) => {
    rows.push(node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return rows;
}

function isCompressed(row) {
  return Number.isFinite(row.chain_count) && row.chain_count > 0;
}

function isZeroDisplay(value) {
  if (value === null || value === undefined) return false;
  if (typeof value === "number") return value === 0;
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "0") return true;
  }
  return false;
}

function hasDisplayValue(value) {
  if (value === null || value === undefined) return false;
  if (typeof value === "number" && value === 0) return false;
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "-") return false;
    if (trimmed === "0") return false;
    if (trimmed.includes("/")) {
      const [left, right] = trimmed.split("/").map((part) => Number(part));
      if (Number.isFinite(left) && Number.isFinite(right)) {
        return !(left === 0 && right === 0);
      }
    }
  }
  return true;
}

function formatSideValue(value, format, decimals) {
  if (!Number.isFinite(value)) return String(value);
  if (value === 0) return "-";
  const parts = formatNumberParts(value, {
    group: format.group,
    decimal: format.decimal,
    decimals,
  });
  return `${parts.sign}${parts.int}${parts.frac ? `${parts.decimal}${parts.frac}` : ""}`;
}

function HeatCell({ row, value, percent, format, numberFormat, dimZero }) {
  const pct = Number.isFinite(percent) ? Math.max(0, Math.min(100, percent)) : 0;
  const alpha = 0.05 + (pct / 100) * 0.22;
  const showHeat = pct > 0;
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const dimmed = isZeroDisplay(value);
  return (
    <div
      className="heat-cell"
      style={{
        background: showHeat
          ? `linear-gradient(90deg, rgba(239, 68, 68, ${alpha}), rgba(220, 38, 38, ${alpha}))`
          : "transparent",
      }}
    >
      <div className={`heat-text${dimmed ? " numeric-zero" : ""}`}>
        {showAggregate ? (
          <span className="aggregate-badge" title="Aggregated via chain compression">
            Σ
          </span>
        ) : null}
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format?.decimals ?? 0}
          className="heat-number"
        />
      </div>
    </div>
  );
}

function NumericCell({ row, value, format, numberFormat, dimZero }) {
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const isPair = typeof value === "string" && value.includes("/") && value.trim() !== "-";
  const dimmed = !isPair && isZeroDisplay(value);
  return (
    <div className={`numeric-cell${dimmed ? " numeric-zero" : ""}`}>
      {showAggregate ? (
        <span className="aggregate-badge" title="Aggregated via chain compression">
          Σ
        </span>
      ) : null}
      {isPair ? (
        <span className="numeric-text">
          {(() => {
            const [leftRaw, rightRaw] = value.split("/");
            const left = Number(leftRaw);
            const right = Number(rightRaw);
            const decimals = format?.decimals ?? 0;
            const leftFormatted = Number.isFinite(left)
              ? formatSideValue(left, numberFormat, decimals)
              : leftRaw;
            const rightFormatted = Number.isFinite(right)
              ? formatSideValue(right, numberFormat, decimals)
              : rightRaw;
            const leftZero = Number.isFinite(left) && left === 0;
            const rightZero = Number.isFinite(right) && right === 0;
            return (
              <>
                <span className={leftZero ? "numeric-zero-part" : undefined}>{leftFormatted}</span>
                <span className="numeric-separator">/</span>
                <span className={rightZero ? "numeric-zero-part" : undefined}>{rightFormatted}</span>
              </>
            );
          })()}
        </span>
      ) : (
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format?.decimals ?? 0}
          className="numeric-text"
        />
      )}
    </div>
  );
}

function HeatHeaderCell({ column, cell, heatConfig, onToggle, onMinChange, onMaxChange, openId, setOpenId }) {
  const heatKey = column.heatKey;
  const isOpen = openId === column.id;
  if (!heatKey) {
    return <div className="heat-header">{cell.text}</div>;
  }
  const config = heatConfig[heatKey] || { enabled: true, min: null, max: null };
  return (
    <div className="heat-header">
      <span>{cell.text}</span>
      <button
        type="button"
        className="heat-header-btn"
        onClick={(event) => {
          event.stopPropagation();
          setOpenId(isOpen ? null : column.id);
        }}
      >
        ⚙
      </button>
      {isOpen ? (
        <div
          className="heat-menu"
          onClick={(event) => {
            event.stopPropagation();
          }}
        >
          <label>
            <input
              type="checkbox"
              checked={config.enabled}
              onChange={() => onToggle(heatKey)}
            />
            Heatmap enabled
          </label>
          <label>
            Min
            <input
              type="number"
              placeholder="auto"
              value={config.min ?? ""}
              onChange={(event) => onMinChange(heatKey, event.target.value)}
            />
          </label>
          <label>
            Max
            <input
              type="number"
              placeholder="auto"
              value={config.max ?? ""}
              onChange={(event) => onMaxChange(heatKey, event.target.value)}
            />
          </label>
        </div>
      ) : null}
    </div>
  );
}

function fetchJson(url) {
  return fetch(url).then((res) => {
    if (!res.ok) {
      return res.text().then((text) => {
        throw new Error(text || `HTTP ${res.status}`);
      });
    }
    return res.json();
  });
}

function SpanCell({ row, onToggleChain, onFocus }) {
  const percent = row.flame_percent ?? 0;
  const label = row.chain_label ?? row.span_name ?? "-";
  const chainCount = row.chain_count ?? 0;
  const hiddenSiblings = row.hidden_siblings ?? 0;
  const tooltipLines = [];
  if (Array.isArray(row.chain_segments) && row.chain_segments.length > 0) {
    row.chain_segments.forEach((segment, index) => {
      const prefix = segment.tag ? `(${segment.tag}): ` : "";
      tooltipLines.push(`${index + 1}. ${prefix}${segment.name}`);
    });
  } else if (row.span_name) {
    const prefix = row.tag ? `(${row.tag}): ` : "";
    tooltipLines.push(`1. ${prefix}${row.span_name}`);
  }
  const tooltip = tooltipLines.join("\n");
  return (
    <div className="span-cell" title={tooltip}>
      <div className="span-bar">
        <div
          className="span-bar-fill"
          style={{
            width: `${percent}%`,
            minWidth: percent > 0 ? "1px" : "0",
          }}
        />
      </div>
      <div className="span-label">
        <span className="span-percent">{percent.toFixed(1)}%</span>
        <span className="span-name">{label}</span>
        {row.tag ? <span className="span-tag">{row.tag}</span> : null}
        {chainCount > 0 ? (
          <button
            type="button"
            className="rounded-full border border-border bg-secondary px-2 py-0.5 text-[10px] text-secondary-foreground"
            onClick={(event) => {
              event.stopPropagation();
              onToggleChain?.(row.id);
            }}
          >
            +{chainCount} frames
          </button>
        ) : null}
        {hiddenSiblings > 0 ? (
          <span className="rounded-full border border-border bg-secondary px-2 py-0.5 text-[10px] text-secondary-foreground">
            +{hiddenSiblings} siblings
          </span>
        ) : null}
        <button
          type="button"
          className="rounded-full border border-border bg-background px-2 py-0.5 text-[10px] text-muted-foreground hover:text-foreground"
          onClick={(event) => {
            event.stopPropagation();
            onFocus?.(row.id);
          }}
        >
          Focus
        </button>
      </div>
    </div>
  );
}

function buildTreeIndex(nodes) {
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

function pruneTree(nodes, minWallUs) {
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

function indexTree(nodes) {
  const byId = new Map();
  const walk = (node) => {
    byId.set(node.id, node);
    (node.data || []).forEach(walk);
  };
  nodes.forEach(walk);
  return byId;
}

function applyFocus(roots, byId, focusId) {
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

function applyHotPath(nodes, mode) {
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

function applyChainCompression(nodes, { enabled, expandedChains, significantSelfUs }) {
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

function applyOpenState(nodes, openNodes, forceOpenAll) {
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

function collectSubtreeIds(node, ids) {
  ids.add(node.id);
  (node.data || []).forEach((child) => collectSubtreeIds(child, ids));
}

function computeDefaultOpenSet(nodes) {
  const openIds = new Set();
  const walk = (node, depth) => {
    const wallMs = (getWallUs(node) ?? 0) / 1000;
    const selfMs = (getSelfWallUs(node) ?? 0) / 1000;
    if (
      depth < DEFAULT_AUTO_EXPAND.depth ||
      selfMs >= DEFAULT_AUTO_EXPAND.selfMs ||
      wallMs >= DEFAULT_AUTO_EXPAND.wallMs
    ) {
      openIds.add(node.id);
    }
    if (node.data && node.data.length > 0) {
      const sorted = [...node.data].sort((a, b) => (getWallUs(b) ?? 0) - (getWallUs(a) ?? 0));
      sorted.slice(0, DEFAULT_AUTO_EXPAND.topKChildren).forEach((child) => openIds.add(child.id));
      node.data.forEach((child) => walk(child, depth + 1));
    }
  };
  nodes.forEach((node) => walk(node, 0));
  return openIds;
}

function ColumnDropdown({ columns, selected, onChange }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="sm" className="gap-2">
          <Columns3 className="h-4 w-4" />
          Columns
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-80 w-56 overflow-auto">
        <DropdownMenuLabel>Visible Columns</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {columns.map((col) => (
          <DropdownMenuCheckboxItem
            key={col.key}
            checked={selected.includes(col.key)}
            onCheckedChange={() => onChange(col.key)}
          >
            {col.label}
          </DropdownMenuCheckboxItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function SettingsPanel({
  open,
  onClose,
  mode,
  setMode,
  minWallMs,
  setMinWallMs,
  limit,
  setLimit,
  hotPathMode,
  setHotPathMode,
  chainCompression,
  setChainCompression,
  numberFormatId,
  setNumberFormatId,
  segmentRootId,
  setSegmentRootId,
  stacksBlockId,
  setStacksBlockId,
  blocks,
  columns,
  selectedColumns,
  toggleColumn,
}) {
  const panelRef = useRef(null);

  useEffect(() => {
    const handleEscape = (e) => {
      if (e.key === "Escape") onClose();
    };
    if (open) {
      document.addEventListener("keydown", handleEscape);
      return () => document.removeEventListener("keydown", handleEscape);
    }
  }, [open, onClose]);

  return (
    <>
      {/* Backdrop */}
      <div
        className={`settings-backdrop ${open ? "settings-backdrop-open" : ""}`}
        onClick={onClose}
      />
      {/* Panel */}
      <div
        ref={panelRef}
        className={`settings-panel ${open ? "settings-panel-open" : ""}`}
      >
        <div className="settings-panel-header">
          <h2 className="text-sm font-semibold text-foreground">Settings</h2>
          <button
            type="button"
            className="settings-close-btn"
            onClick={onClose}
            aria-label="Close settings"
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M4 4l8 8M12 4l-8 8" />
            </svg>
          </button>
        </div>

        <div className="settings-panel-content">
          {/* Query Mode */}
          <div className="settings-section">
            <h3 className="settings-section-title">Query Mode</h3>
            <div className="settings-mode-toggle">
              <button
                type="button"
                className={`settings-mode-btn ${mode === "tx" ? "settings-mode-btn-active" : ""}`}
                onClick={() => setMode("tx")}
              >
                Transaction
              </button>
              <button
                type="button"
                className={`settings-mode-btn ${mode === "run" ? "settings-mode-btn-active" : ""}`}
                onClick={() => setMode("run")}
              >
                Run Scope
              </button>
            </div>
          </div>

          {/* Query Filters */}
          <div className="settings-section">
            <h3 className="settings-section-title">Query Filters</h3>
            <div className="settings-field">
              <label className="settings-label">Min Wall Time (ms)</label>
              <input
                type="text"
                className="settings-input"
                value={minWallMs}
                onChange={(e) => setMinWallMs(e.target.value)}
                placeholder="e.g. 1"
              />
            </div>
            <div className="settings-field">
              <label className="settings-label">Record Limit</label>
              <input
                type="text"
                className="settings-input"
                value={limit}
                onChange={(e) => setLimit(e.target.value)}
                placeholder="5000"
              />
            </div>
            {mode === "run" && (
              <>
                <div className="settings-field">
                  <label className="settings-label">Stacks Block</label>
                  <select
                    className="settings-input"
                    value={stacksBlockId}
                    onChange={(e) => setStacksBlockId(e.target.value)}
                  >
                    <option value="">Any</option>
                    {blocks.map((block) => (
                      <option key={block.stacks_block_id} value={block.stacks_block_id}>
                        #{block.height} · {block.block_hash_hex?.slice(0, 12)}...
                      </option>
                    ))}
                  </select>
                </div>
                <div className="settings-field">
                  <label className="settings-label">Segment Root ID</label>
                  <input
                    type="text"
                    className="settings-input"
                    value={segmentRootId}
                    onChange={(e) => setSegmentRootId(e.target.value)}
                    placeholder="Optional"
                  />
                </div>
              </>
            )}
          </div>

          {/* Display Options */}
          <div className="settings-section">
            <h3 className="settings-section-title">Display</h3>
            <div className="settings-field">
              <label className="settings-label">Hot Path</label>
              <select
                className="settings-input"
                value={hotPathMode}
                onChange={(e) => setHotPathMode(e.target.value)}
              >
                <option value="off">Off</option>
                <option value="inclusive">Inclusive Time</option>
                <option value="self">Self Time</option>
              </select>
            </div>
            <div className="settings-field">
              <label className="settings-label">Number Format</label>
              <select
                className="settings-input"
                value={numberFormatId}
                onChange={(e) => setNumberFormatId(e.target.value)}
              >
                {NUMBER_FORMATS.map((format) => (
                  <option key={format.id} value={format.id}>
                    {format.label}
                  </option>
                ))}
              </select>
            </div>
            <label className="settings-checkbox">
              <input
                type="checkbox"
                checked={chainCompression}
                onChange={(e) => setChainCompression(e.target.checked)}
              />
              <span>Compress linear chains</span>
            </label>
          </div>

          {/* Columns */}
          <div className="settings-section">
            <h3 className="settings-section-title">Visible Columns</h3>
            <div className="settings-columns-grid">
              {columns.map((col) => (
                <label key={col.key} className="settings-checkbox">
                  <input
                    type="checkbox"
                    checked={selectedColumns.includes(col.key)}
                    onChange={() => toggleColumn(col.key)}
                  />
                  <span>{col.label}</span>
                </label>
              ))}
            </div>
          </div>
        </div>
      </div>
    </>
  );
}

export default function App() {
  const [runs, setRuns] = useState([]);
  const [blocks, setBlocks] = useState([]);
  const [runId, setRunId] = useState("");
  const [mode, setMode] = useState("tx");
  const [txQuery, setTxQuery] = useState("");
  const [stacksTxId, setStacksTxId] = useState("");
  const [stacksBlockId, setStacksBlockId] = useState("");
  const [minWallMs, setMinWallMs] = useState("");
  const [segmentRootId, setSegmentRootId] = useState("");
  const [limit, setLimit] = useState("5000");
  const [rows, setRows] = useState([]);
  const [summary, setSummary] = useState("");
  const [chainCompression, setChainCompression] = useState(true);
  const [hotPathMode, setHotPathMode] = useState("off");
  const [focusId, setFocusId] = useState(null);
  const [activeId, setActiveId] = useState(null);
  const [openNodes, setOpenNodes] = useState(new Set());
  const [expandedChains, setExpandedChains] = useState(new Set());
  const [heatMenuOpenId, setHeatMenuOpenId] = useState(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [theme, setTheme] = useState(() => {
    return localStorage.getItem("profilerTheme") || "dark";
  });
  const [isLoading, setIsLoading] = useState(false);
  const [lastLoadedQuery, setLastLoadedQuery] = useState(null);
  const [heatConfig, setHeatConfig] = useState(() => {
    const stored = localStorage.getItem("profilerHeatConfig");
    if (stored) {
      try {
        return JSON.parse(stored);
      } catch {
        return {};
      }
    }
    return {
      wallTotalUs: { enabled: true, min: null, max: null },
      selfWallUs: { enabled: true, min: null, max: null },
      busyTotalUs: { enabled: true, min: null, max: null },
      selfBusyUs: { enabled: true, min: null, max: null },
      waitTotalUs: { enabled: true, min: null, max: null },
      selfWaitUs: { enabled: true, min: null, max: null },
      clarityRuntime: { enabled: true, min: null, max: null },
    };
  });
  const [selectedColumns, setSelectedColumns] = useState(() => {
    const stored = localStorage.getItem("profilerColumns");
    if (!stored) return sanitizeSelectedColumns(DEFAULT_COLUMNS);
    try {
      const parsed = JSON.parse(stored);
      return sanitizeSelectedColumns(parsed);
    } catch {
      return sanitizeSelectedColumns(DEFAULT_COLUMNS);
    }
  });
  const [numberFormatId, setNumberFormatId] = useState(() => {
    return localStorage.getItem("profilerNumberFormat") || DEFAULT_NUMBER_FORMAT_ID;
  });
  const numberFormat = useMemo(() => getNumberFormat(numberFormatId), [numberFormatId]);

  const gridApiRef = useRef(null);

  useEffect(() => {
    localStorage.setItem("profilerHeatConfig", JSON.stringify(heatConfig));
  }, [heatConfig]);

  useEffect(() => {
    localStorage.setItem("profilerTheme", theme);
  }, [theme]);

  useEffect(() => {
    const root = document.documentElement;
    if (theme === "dark") {
      root.classList.add("dark");
    } else {
      root.classList.remove("dark");
    }
  }, [theme]);

  const toggleTheme = useCallback(() => {
    setTheme((prev) => (prev === "dark" ? "light" : "dark"));
  }, []);

  useEffect(() => {
    localStorage.setItem("profilerNumberFormat", numberFormatId);
  }, [numberFormatId]);

  useEffect(() => {
    const handleClick = () => setHeatMenuOpenId(null);
    document.addEventListener("click", handleClick);
    return () => document.removeEventListener("click", handleClick);
  }, []);

  useEffect(() => {
    fetchJson("/api/runs")
      .then((data) => {
        setRuns(data);
        if (data.length > 0) setRunId(String(data[0].id));
      })
      .catch((err) => setSummary(err.message));
  }, []);

  useEffect(() => {
    if (!runId) return;
    fetchJson(`/api/blocks?run_id=${runId}`)
      .then(setBlocks)
      .catch(() => setBlocks([]));
  }, [runId]);

  useEffect(() => {
    if (!runId) return;
    const normalized = txQuery.trim().toLowerCase();
    if (normalized.length !== 64) return;
    fetchJson(`/api/tx-lookup?run_id=${runId}&tx_hash=${normalized}`)
      .then((data) => {
        if (data?.stacks_tx_id) {
          setStacksTxId(String(data.stacks_tx_id));
        } else {
          setSummary("Tx hash not found in this run.");
        }
      })
      .catch((err) => setSummary(err.message));
  }, [runId, txQuery]);

  const loadTrace = useCallback(async () => {
    if (!runId) return;
    const url = new URL("/api/trace", window.location.origin);
    url.searchParams.set("run_id", runId);
    url.searchParams.set("mode", mode);
    url.searchParams.set("limit", limit || "5000");
    if (mode === "tx" && stacksTxId) {
      url.searchParams.set("stacks_tx_id", stacksTxId);
    }
    if (mode === "run") {
      if (stacksBlockId) url.searchParams.set("stacks_block_id", stacksBlockId);
      if (segmentRootId) url.searchParams.set("segment_root_id", segmentRootId);
      if (minWallMs) url.searchParams.set("min_wall_ms", minWallMs);
    }

    setSummary("Loading trace...");
    setIsLoading(true);
    try {
      const data = await fetchJson(url.toString());
      setRows(data);
      setSummary(`${data.length} records loaded.`);
    } finally {
      setIsLoading(false);
    }
    setLastLoadedQuery({
      runId,
      mode,
      txQuery,
      stacksTxId,
      stacksBlockId,
      segmentRootId,
      minWallMs,
      limit,
      hotPathMode,
    });
  }, [runId, mode, limit, stacksTxId, stacksBlockId, segmentRootId, minWallMs, txQuery, hotPathMode]);

  const isDirty = useMemo(() => {
    if (!lastLoadedQuery) return true;
    return (
      lastLoadedQuery.runId !== runId ||
      lastLoadedQuery.mode !== mode ||
      lastLoadedQuery.txQuery !== txQuery ||
      lastLoadedQuery.stacksTxId !== stacksTxId ||
      lastLoadedQuery.stacksBlockId !== stacksBlockId ||
      lastLoadedQuery.segmentRootId !== segmentRootId ||
      lastLoadedQuery.minWallMs !== minWallMs ||
      lastLoadedQuery.limit !== limit ||
      lastLoadedQuery.hotPathMode !== hotPathMode
    );
  }, [lastLoadedQuery, runId, mode, txQuery, stacksTxId, stacksBlockId, segmentRootId, minWallMs, limit, hotPathMode]);

  const resetQuery = useCallback(() => {
    setMode("tx");
    setTxQuery("");
    setStacksTxId("");
    setStacksBlockId("");
    setSegmentRootId("");
    setMinWallMs("");
    setLimit("5000");
    setHotPathMode("off");
    setChainCompression(true);
  }, []);

  const maxWallUs = useMemo(() => {
    return rows.reduce((maxVal, row) => {
      const wall = getWallUs(row);
      if (wall == null) return maxVal;
      return Math.max(maxVal, wall);
    }, 0);
  }, [rows]);

  const baseRows = useMemo(() => {
    return rows.map((row) => ({
      ...row,
      flame_percent: maxWallUs > 0 ? ((getWallUs(row) ?? 0) / maxWallUs) * 100 : 0,
    }));
  }, [rows, maxWallUs]);

  const baseTree = useMemo(() => buildTreeIndex(baseRows), [baseRows]);

  const minWallUs = useMemo(() => (minWallMs ? Number(minWallMs) * 1000 : null), [minWallMs]);

  const prunedRoots = useMemo(
    () => pruneTree(baseTree.roots, minWallUs),
    [baseTree.roots, minWallUs]
  );

  const prunedById = useMemo(() => indexTree(prunedRoots), [prunedRoots]);

  const focusedTree = useMemo(
    () => applyFocus(prunedRoots, prunedById, focusId),
    [prunedRoots, prunedById, focusId]
  );

  const hotPathRoots = useMemo(
    () => applyHotPath(focusedTree.roots, hotPathMode),
    [focusedTree.roots, hotPathMode]
  );

  const compressedRoots = useMemo(
    () =>
      applyChainCompression(hotPathRoots, {
        enabled: chainCompression,
        expandedChains,
        significantSelfUs: DEFAULT_AUTO_EXPAND.selfMs * 1000,
      }),
    [hotPathRoots, chainCompression, expandedChains]
  );

  const withFlamePercent = useMemo(() => {
    const applyPercent = (node) => {
      const wall = getWallUs(node) ?? 0;
      const flame_percent = maxWallUs > 0 ? (wall / maxWallUs) * 100 : 0;
      return {
        ...node,
        flame_percent,
        data: (node.data || []).map(applyPercent),
      };
    };
    return compressedRoots.map(applyPercent);
  }, [compressedRoots, maxWallUs]);

  const roots = useMemo(
    () => applyOpenState(withFlamePercent, openNodes, hotPathMode !== "off"),
    [withFlamePercent, openNodes, hotPathMode]
  );

  const heatMax = useMemo(() => {
    const flat = flattenTree(withFlamePercent);
    return flat.reduce(
      (acc, row) => {
        const wallTotal = getWallUs(row) ?? 0;
        const busyTotal = row.est_cpu_us ?? row.cpu_time_us ?? 0;
        const waitTotal = Math.max(0, wallTotal - busyTotal);
        const selfWall = getSelfWallUs(row) ?? 0;
        const selfBusy = getSelfCpuUs(row) ?? 0;
        const selfWait = getSelfWaitUs(row) ?? 0;
        const clarity = typeof row.clarity_runtime === "number" ? row.clarity_runtime : 0;
        acc.wallTotalUs = Math.max(acc.wallTotalUs, wallTotal);
        acc.busyTotalUs = Math.max(acc.busyTotalUs, busyTotal);
        acc.waitTotalUs = Math.max(acc.waitTotalUs, waitTotal);
        acc.selfWallUs = Math.max(acc.selfWallUs, selfWall);
        acc.selfBusyUs = Math.max(acc.selfBusyUs, selfBusy);
        acc.selfWaitUs = Math.max(acc.selfWaitUs, selfWait);
        acc.clarityRuntime = Math.max(acc.clarityRuntime, clarity);
        return acc;
      },
      {
        wallTotalUs: 0,
        busyTotalUs: 0,
        waitTotalUs: 0,
        selfWallUs: 0,
        selfBusyUs: 0,
        selfWaitUs: 0,
        clarityRuntime: 0,
      }
    );
  }, [withFlamePercent]);

  const getHeatBounds = useCallback(
    (heatKey) => {
      const config = heatConfig[heatKey] || {};
      const min = config.min ?? 0;
      const max = config.max ?? heatMax[heatKey] ?? 0;
      return { min, max, enabled: config.enabled !== false };
    },
    [heatConfig, heatMax]
  );

  const toggleHeat = useCallback((heatKey) => {
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        enabled: !(prev[heatKey]?.enabled ?? true),
      },
    }));
  }, []);

  const setHeatMin = useCallback((heatKey, value) => {
    const next = value === "" ? null : Number(value);
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        min: Number.isNaN(next) ? null : next,
      },
    }));
  }, []);

  const setHeatMax = useCallback((heatKey, value) => {
    const next = value === "" ? null : Number(value);
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        max: Number.isNaN(next) ? null : next,
      },
    }));
  }, []);

  const breadcrumb = focusedTree.breadcrumb ?? [];

  useEffect(() => {
    setOpenNodes(computeDefaultOpenSet(focusedTree.roots));
  }, [rows, focusId, minWallMs, focusedTree.roots]);

  const toggleChain = useCallback((rowId) => {
    setExpandedChains((prev) => {
      const next = new Set(prev);
      if (next.has(rowId)) {
        next.delete(rowId);
      } else {
        next.add(rowId);
      }
      return next;
    });
  }, []);

  const focusNode = useCallback((rowId) => {
    setFocusId(rowId);
    setActiveId(rowId);
  }, []);

  const clearFocus = useCallback(() => {
    setFocusId(null);
  }, []);

  const expandToDepth = useCallback(
    (depth) => {
      const openIds = new Set();
      const walk = (node, currentDepth) => {
        if (currentDepth < depth) openIds.add(node.id);
        if (currentDepth < depth) {
          (node.data || []).forEach((child) => walk(child, currentDepth + 1));
        }
      };
      focusedTree.roots.forEach((node) => walk(node, 0));
      setOpenNodes(openIds);
    },
    [focusedTree.roots]
  );

  const collapseSiblings = useCallback(() => {
    if (!activeId) return;
    const activeNode = prunedById.get(activeId);
    if (!activeNode || !activeNode.parent_id) return;
    const parent = prunedById.get(activeNode.parent_id);
    if (!parent) return;
    const next = new Set(openNodes);
    parent.data.forEach((child) => {
      if (child.id !== activeId) {
        const ids = new Set();
        collectSubtreeIds(child, ids);
        ids.forEach((id) => next.delete(id));
      }
    });
    next.add(parent.id);
    next.add(activeId);
    setOpenNodes(next);
  }, [activeId, prunedById, openNodes]);

  const buildHeatHeader = useCallback(
    (col) => ({
      text: col.headerLabel ?? col.label,
      cell: (props) => (
        <HeatHeaderCell
          {...props}
          heatConfig={heatConfig}
          onToggle={toggleHeat}
          onMinChange={setHeatMin}
          onMaxChange={setHeatMax}
          openId={heatMenuOpenId}
          setOpenId={setHeatMenuOpenId}
        />
      ),
    }),
    [heatConfig, heatMenuOpenId, setHeatMax, setHeatMin, toggleHeat]
  );

  const buildGroupHeader = useCallback(
    (col) => [
      col.groupStart
        ? { text: col.group, colspan: col.groupSpan, css: "grid-group-header" }
        : { text: "", _hidden: true },
      col.heatKey ? buildHeatHeader(col) : { text: col.headerLabel ?? col.label },
    ],
    [buildHeatHeader]
  );

  const visibleColumns = useMemo(() => {
    return COLUMN_DEFS.map((col) => ({
      id: col.key,
      header: col.group
        ? buildGroupHeader(col)
        : col.key === "clarity_rw" ||
            col.key === "clarity_len" ||
            col.key === "clarity_runtime" ||
            col.key === "clarity_input_n"
          ? [
              col.key === "clarity_rw"
                ? { text: "Clarity", colspan: 4, css: "grid-group-header" }
                : { text: "", _hidden: true },
              col.heatKey ? buildHeatHeader(col) : { text: col.label },
            ]
          : col.heatKey
            ? buildHeatHeader(col)
            : col.label,
      width: col.width,
      flexgrow: col.flexgrow,
      heatKey: col.heatKey,
      treetoggle: col.treetoggle,
      cell:
        col.key === "span"
          ? (props) => <SpanCell {...props} onToggleChain={toggleChain} onFocus={focusNode} />
          : col.heatKey
            ? (props) => {
                const raw =
                  col.heatKey === "wallTotalUs"
                    ? getWallUs(props.row)
                    : col.heatKey === "busyTotalUs"
                      ? props.row.est_cpu_us ?? props.row.cpu_time_us ?? null
                      : col.heatKey === "waitTotalUs"
                        ? (() => {
                            const wall = getWallUs(props.row);
                            const cpu = props.row.est_cpu_us ?? props.row.cpu_time_us ?? null;
                            if (wall == null || cpu == null) return null;
                            return Math.max(0, wall - cpu);
                          })()
                        : col.heatKey === "selfWallUs"
                    ? getSelfWallUs(props.row)
                    : col.heatKey === "selfBusyUs"
                      ? getSelfCpuUs(props.row)
                      : col.heatKey === "selfWaitUs"
                        ? getSelfWaitUs(props.row)
                        : typeof props.row.clarity_runtime === "number"
                          ? props.row.clarity_runtime
                          : null;
                const bounds = getHeatBounds(col.heatKey);
                const max = bounds.max;
                const min = bounds.min;
                const pct =
                  bounds.enabled && raw != null && raw > 0 && max > min
                    ? ((raw - min) / (max - min)) * 100
                    : 0;
                const value = col.getter ? col.getter(props.row) : props.row[col.key] ?? "-";
                return (
                  <HeatCell
                    row={props.row}
                    value={value}
                    percent={pct}
                    format={col.format}
                    numberFormat={numberFormat}
                    dimZero={COUNT_DIM_KEYS.has(col.key)}
                  />
                );
              }
            : NUMERIC_COLUMN_KEYS.has(col.key)
              ? (props) => {
                  const value = col.getter ? col.getter(props.row) : props.row[col.key] ?? "-";
                  return (
                    <NumericCell
                      row={props.row}
                      value={value}
                      format={col.format}
                      numberFormat={numberFormat}
                      dimZero={COUNT_DIM_KEYS.has(col.key)}
                    />
                  );
                }
              : col.cell,
      getter: col.getter,
      hidden: col.alwaysHidden
        ? true
        : col.alwaysVisible
          ? false
          : selectedColumns.includes(col.key)
            ? false
            : true,
      resize: true,
    }));
  }, [selectedColumns, toggleChain, focusNode, buildHeatHeader, buildGroupHeader, getHeatBounds, numberFormat]);

  const toggleColumn = (key) => {
    setSelectedColumns((prev) => {
      if (ALWAYS_VISIBLE_KEYS.has(key) || ALWAYS_HIDDEN_KEYS.has(key)) return prev;
      const columnDef = COLUMN_DEFS.find((col) => col.key === key);
      if (!columnDef || columnDef.selectable === false) return prev;
      const next = prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key];
      const sanitized = sanitizeSelectedColumns(next);
      localStorage.setItem("profilerColumns", JSON.stringify(sanitized));
      return sanitized;
    });
  };

  return (
    <div className={`app-container ${theme}`}>
      {/* Settings Panel */}
      <SettingsPanel
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        mode={mode}
        setMode={setMode}
        minWallMs={minWallMs}
        setMinWallMs={setMinWallMs}
        limit={limit}
        setLimit={setLimit}
        hotPathMode={hotPathMode}
        setHotPathMode={setHotPathMode}
        chainCompression={chainCompression}
        setChainCompression={setChainCompression}
        numberFormatId={numberFormatId}
        setNumberFormatId={setNumberFormatId}
        segmentRootId={segmentRootId}
        setSegmentRootId={setSegmentRootId}
        stacksBlockId={stacksBlockId}
        setStacksBlockId={setStacksBlockId}
        blocks={blocks}
        columns={SELECTABLE_COLUMNS}
        selectedColumns={selectedColumns}
        toggleColumn={toggleColumn}
      />

      {/* Header */}
      <header className="app-header">
        <div className="header-left">
          <h1 className="header-title">Profiler Explorer</h1>
          <div className="header-divider" />
          <Select value={runId} onValueChange={setRunId}>
            <SelectTrigger className="w-[200px]">
              <SelectValue placeholder="Select a run" />
            </SelectTrigger>
            <SelectContent>
              {runs.map((run) => (
                <SelectItem key={run.id} value={String(run.id)}>
                  {run.run_name ? `${run.id} · ${run.run_name}` : `Run ${run.id}`}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        <div className="header-center">
          <div className={`search-container ${txQuery && !/^[0-9a-fA-F]{0,64}$/.test(txQuery) ? "search-container-invalid" : ""}`}>
            <svg className="search-icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5">
              <circle cx="7" cy="7" r="5" />
              <path d="M11 11l3 3" />
            </svg>
            <input
              type="text"
              className="search-input"
              value={txQuery}
              onChange={(e) => {
                const value = e.target.value.trim();
                if (value === "" || /^[0-9a-fA-F]{0,64}$/.test(value)) {
                  setTxQuery(value);
                }
              }}
              placeholder={mode === "tx" ? "Enter transaction hash (64 hex chars)..." : "Search..."}
              maxLength={64}
              spellCheck={false}
            />
            {txQuery && (
              <>
                <span className={`search-length ${txQuery.length === 64 ? "search-length-valid" : ""}`}>
                  {txQuery.length}/64
                </span>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6 mr-1"
                  onClick={() => setTxQuery("")}
                >
                  <X className="h-3 w-3" />
                </Button>
              </>
            )}
          </div>
        </div>

        <div className="header-right">
          <div className="header-stats">
            <span className="stat-badge">{rows.length.toLocaleString()} rows</span>
          </div>
          <TooltipProvider delayDuration={300}>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="outline" size="icon" onClick={toggleTheme}>
                  {theme === "dark" ? (
                    <Sun className="h-4 w-4" />
                  ) : (
                    <Moon className="h-4 w-4" />
                  )}
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                {theme === "dark" ? "Switch to light mode" : "Switch to dark mode"}
              </TooltipContent>
            </Tooltip>
          </TooltipProvider>
          <TooltipProvider delayDuration={300}>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="outline" size="icon" onClick={resetQuery}>
                  <RotateCcw className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Reset all filters</TooltipContent>
            </Tooltip>
          </TooltipProvider>
          <TooltipProvider delayDuration={300}>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="outline" size="icon" onClick={() => setSettingsOpen(true)}>
                  <Settings className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Settings</TooltipContent>
            </Tooltip>
          </TooltipProvider>
          <Button onClick={loadTrace} disabled={!isDirty || isLoading}>
            {isLoading ? (
              <>
                <Loader2 className="h-4 w-4 animate-spin" />
                Loading...
              </>
            ) : (
              "Load Trace"
            )}
          </Button>
        </div>
      </header>

      {/* Toolbar */}
      <div className="app-toolbar">
        <div className="toolbar-left">
          <ColumnDropdown
            columns={SELECTABLE_COLUMNS}
            selected={selectedColumns}
            onChange={toggleColumn}
          />
          <div className="toolbar-divider" />
          <div className="toolbar-group">
            <span className="toolbar-label">Depth:</span>
            {[2, 4, 6].map((depth) => (
              <Button
                key={depth}
                variant="outline"
                size="sm"
                onClick={() => expandToDepth(depth)}
                disabled={hotPathMode !== "off"}
              >
                {depth}
              </Button>
            ))}
          </div>
          <Button
            variant="outline"
            size="sm"
            onClick={collapseSiblings}
            disabled={!activeId}
          >
            Collapse siblings
          </Button>
          {focusId && (
            <Button
              variant="outline"
              size="sm"
              onClick={clearFocus}
              className="border-primary text-primary hover:bg-primary/10"
            >
              Clear focus
            </Button>
          )}
        </div>
        <div className="toolbar-right">
          {summary && <span className="toolbar-status">{summary}</span>}
        </div>
      </div>

      {/* Breadcrumb */}
      {breadcrumb.length > 0 && (
        <div className="breadcrumb-bar">
          <span className="breadcrumb-label">Focus:</span>
          {breadcrumb.map((node, index) => (
            <span key={node.id} className="breadcrumb-item">
              {node.span_name ?? `Node ${node.id}`}
              {index < breadcrumb.length - 1 && <span className="breadcrumb-separator">›</span>}
            </span>
          ))}
        </div>
      )}

      {/* Grid */}
      <section className="app-grid-container">
        <WillowDark>
          <Grid
            tree={true}
            data={roots}
            columns={visibleColumns}
            sizes={{ rowHeight: 36 }}
            select={true}
            rowStyle={() => "profiler-row"}
            columnStyle={(column) =>
              NUMERIC_COLUMN_KEYS.has(column.id)
                ? "grid-col-numeric"
                : column.id === "span"
                  ? "grid-col-span"
                  : ""
            }
            init={(api) => {
              gridApiRef.current = api;
            }}
            onOpenRow={(ev) => {
              setOpenNodes((prev) => new Set(prev).add(ev.id));
            }}
            onCloseRow={(ev) => {
              setOpenNodes((prev) => {
                const next = new Set(prev);
                next.delete(ev.id);
                return next;
              });
            }}
            onSelectRow={(ev) => {
              if (ev?.id != null) setActiveId(ev.id);
            }}
          />
        </WillowDark>
      </section>
    </div>
  );
}
