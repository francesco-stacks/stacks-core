import { toAvgMs, toMs } from "./treeTransforms.ts";

type ColumnFormat = {
  decimals?: number;
};

type ColumnDef = {
  key: string;
  label: string;
  width?: number;
  flexgrow?: number;
  default?: boolean;
  selectable?: boolean;
  alwaysVisible?: boolean;
  alwaysHidden?: boolean;
  treetoggle?: boolean;
  headerLabel?: string;
  group?: string;
  groupSpan?: number;
  groupStart?: boolean;
  heatKey?: string;
  format?: ColumnFormat;
  getter?: (row: Record<string, any>) => any;
};

export const COLUMN_DEFS: ColumnDef[] = [
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

export const NUMERIC_COLUMN_KEYS = new Set([
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

export const COUNT_DIM_KEYS = new Set([
  "clarity_rw",
  "clarity_len",
  "clarity_runtime",
  "clarity_input_n",
]);

export const ALWAYS_VISIBLE_KEYS = new Set(
  COLUMN_DEFS.filter((c) => c.alwaysVisible).map((c) => c.key)
);

export const ALWAYS_HIDDEN_KEYS = new Set(
  COLUMN_DEFS.filter((c) => c.alwaysHidden).map((c) => c.key)
);

export const SELECTABLE_COLUMNS = COLUMN_DEFS.filter((c) => c.selectable !== false);

export const DEFAULT_COLUMNS = COLUMN_DEFS.filter((c) => c.default)
  .map((c) => c.key)
  .filter((key) => !ALWAYS_HIDDEN_KEYS.has(key));

export const sanitizeSelectedColumns = (values: unknown) => {
  const base = Array.isArray(values)
    ? values.filter((key) => !ALWAYS_HIDDEN_KEYS.has(String(key)))
    : [];
  ALWAYS_VISIBLE_KEYS.forEach((key) => {
    if (!base.includes(key)) base.push(key);
  });
  return base.length > 0 ? base : DEFAULT_COLUMNS;
};
